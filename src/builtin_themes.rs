//! Synchronizes platform-owned built-in theme CSS into an existing database.
//!
//! Built-in themes live in `themes` rows with `org_id IS NULL` and are keyed by their
//! platform slug. Fresh-database bootstrap seeds them once with `INSERT OR IGNORE`, so a
//! changed built-in stylesheet (Canuck, for example) never reaches a database that
//! already has the row. This module closes that gap for normal deployments: the
//! canonical values come from the release-owned bootstrap source, and only platform rows
//! are touched. Organization-owned themes, their IDs, assignments, and usage counts stay
//! exactly as they were.
//!
//! The bootstrap source is SQL, so it is applied to a scratch database and read back
//! rather than parsed by hand. That keeps one source of truth: whatever
//! `db/bootstrap/themes.sql` says is what the sync writes.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

/// Bootstrap seed file that owns the built-in theme values.
pub const BUILTIN_THEMES_FILE: &str = "themes.sql";

/// Outcome of one sync. Slugs are reported so a deployment log names what changed.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct BuiltinThemeSyncReport {
    pub updated: Vec<String>,
    pub inserted: Vec<String>,
}

impl BuiltinThemeSyncReport {
    pub fn changed(&self) -> bool {
        !self.updated.is_empty() || !self.inserted.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuiltinThemeError {
    #[error("could not open SQLite database {path}: {source}")]
    DatabaseOpenError {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("missing built-in theme source: {path}")]
    MissingSource { path: PathBuf },

    #[error("could not read built-in theme source {path}: {source}")]
    SourceReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("built-in theme source {path} is invalid: {source}")]
    SourceSqlError {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("built-in theme source {path} defines no themes")]
    NoThemes { path: PathBuf },

    #[error("built-in theme sync failed for {slug}: {source}")]
    SyncSqlError { slug: String, source: rusqlite::Error },

    #[error("built-in theme {slug} did not match the source after sync: database has {actual:?}, source has {expected:?}")]
    VerificationFailed {
        slug: String,
        expected: String,
        actual: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuiltinTheme {
    slug: String,
    name: String,
    css: String,
    created_at: String,
    updated_at: String,
}

pub fn sync_builtin_themes_from(
    database_path: impl AsRef<Path>,
    bootstrap_dir: impl AsRef<Path>,
) -> Result<BuiltinThemeSyncReport, BuiltinThemeError> {
    let mut conn = Connection::open(database_path.as_ref()).map_err(|source| {
        BuiltinThemeError::DatabaseOpenError {
            path: database_path.as_ref().to_path_buf(),
            source,
        }
    })?;
    sync_builtin_themes_connection(&mut conn, bootstrap_dir)
}

pub fn sync_builtin_themes_connection(
    conn: &mut Connection,
    bootstrap_dir: impl AsRef<Path>,
) -> Result<BuiltinThemeSyncReport, BuiltinThemeError> {
    let themes = read_canonical_themes(bootstrap_dir.as_ref())?;
    apply_canonical_themes(conn, &themes)
}

/// Writes every canonical theme in one transaction. A failure rolls the whole sync back,
/// so the database keeps its previous built-in CSS rather than a half-updated set.
fn apply_canonical_themes(
    conn: &mut Connection,
    themes: &[BuiltinTheme],
) -> Result<BuiltinThemeSyncReport, BuiltinThemeError> {
    let transaction = conn
        .transaction()
        .map_err(|source| BuiltinThemeError::SyncSqlError {
            slug: "built-in theme transaction".into(),
            source,
        })?;
    let mut report = BuiltinThemeSyncReport::default();
    for theme in themes {
        match sync_one_theme(&transaction, theme)? {
            ThemeChange::Updated => report.updated.push(theme.slug.clone()),
            ThemeChange::Inserted => report.inserted.push(theme.slug.clone()),
            ThemeChange::Unchanged => {}
        }
    }
    // Verify before committing so a mismatch rolls the whole sync back instead of leaving
    // a database that the deployment then refuses to activate.
    verify_canonical_themes(&transaction, themes)?;
    transaction
        .commit()
        .map_err(|source| BuiltinThemeError::SyncSqlError {
            slug: "built-in theme transaction".into(),
            source,
        })?;
    Ok(report)
}

enum ThemeChange {
    Updated,
    Inserted,
    Unchanged,
}

/// Applies the bootstrap source to a scratch database so SQLite parses it, then reads the
/// canonical rows back in file order.
fn read_canonical_themes(
    bootstrap_dir: &Path,
) -> Result<Vec<BuiltinTheme>, BuiltinThemeError> {
    let path = bootstrap_dir.join(BUILTIN_THEMES_FILE);
    if !path.is_file() {
        return Err(BuiltinThemeError::MissingSource { path });
    }
    let source = std::fs::read_to_string(&path).map_err(|source| {
        BuiltinThemeError::SourceReadError {
            path: path.clone(),
            source,
        }
    })?;

    let scratch = Connection::open_in_memory().map_err(|source| BuiltinThemeError::SourceSqlError {
        path: path.clone(),
        source,
    })?;
    scratch
        .execute_batch(SCRATCH_THEMES_TABLE)
        .map_err(|source| BuiltinThemeError::SourceSqlError {
            path: path.clone(),
            source,
        })?;
    scratch
        .execute_batch(&source)
        .map_err(|source| BuiltinThemeError::SourceSqlError {
            path: path.clone(),
            source,
        })?;

    let mut statement = scratch
        .prepare("SELECT slug, name, css, created_at, updated_at FROM themes ORDER BY id")
        .map_err(|source| BuiltinThemeError::SourceSqlError {
            path: path.clone(),
            source,
        })?;
    let themes = statement
        .query_map([], |row| {
            Ok(BuiltinTheme {
                slug: row.get(0)?,
                name: row.get(1)?,
                css: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })
        .map_err(|source| BuiltinThemeError::SourceSqlError {
            path: path.clone(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| BuiltinThemeError::SourceSqlError {
            path: path.clone(),
            source,
        })?;

    if themes.is_empty() {
        return Err(BuiltinThemeError::NoThemes { path });
    }
    Ok(themes)
}

/// Writes one canonical theme into the platform row with that slug. A row that already
/// matches the source is left alone, so a repeated sync reports no work and does not
/// churn `updated_at`.
fn sync_one_theme(conn: &Connection, theme: &BuiltinTheme) -> Result<ThemeChange, BuiltinThemeError> {
    let updated = conn
        .execute(
            "UPDATE themes
                SET name = ?2, css = ?3, updated_at = ?4
              WHERE org_id IS NULL AND slug = ?1
                AND (name IS NOT ?2 OR css IS NOT ?3)",
            rusqlite::params![theme.slug, theme.name, theme.css, theme.updated_at],
        )
        .map_err(|source| BuiltinThemeError::SyncSqlError {
            slug: theme.slug.clone(),
            source,
        })?;
    if updated > 0 {
        return Ok(ThemeChange::Updated);
    }

    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM themes WHERE org_id IS NULL AND slug = ?1",
            [&theme.slug],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| BuiltinThemeError::SyncSqlError {
            slug: theme.slug.clone(),
            source,
        })?;
    if existing.is_some() {
        return Ok(ThemeChange::Unchanged);
    }

    conn.execute(
        "INSERT INTO themes (org_id, slug, name, css, created_at, updated_at)
         VALUES (NULL, ?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            theme.slug,
            theme.name,
            theme.css,
            theme.created_at,
            theme.updated_at
        ],
    )
    .map_err(|source| BuiltinThemeError::SyncSqlError {
        slug: theme.slug.clone(),
        source,
    })?;
    Ok(ThemeChange::Inserted)
}

/// Every canonical slug must match the database after the write, so a partial or blocked
/// update fails deployment preparation instead of leaving stale built-in CSS in place.
fn verify_canonical_themes(
    conn: &Connection,
    themes: &[BuiltinTheme],
) -> Result<(), BuiltinThemeError> {
    for theme in themes {
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT name, css FROM themes WHERE org_id IS NULL AND slug = ?1",
                [&theme.slug],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|source| BuiltinThemeError::SyncSqlError {
                slug: theme.slug.clone(),
                source,
            })?;
        match row {
            Some((name, css)) if name == theme.name && css == theme.css => {}
            Some((name, css)) => {
                return Err(BuiltinThemeError::VerificationFailed {
                    slug: theme.slug.clone(),
                    expected: format!("{}/{}-byte css", theme.name, theme.css.len()),
                    actual: format!("{}/{}-byte css", name, css.len()),
                })
            }
            None => {
                return Err(BuiltinThemeError::VerificationFailed {
                    slug: theme.slug.clone(),
                    expected: format!("{}/{}-byte css", theme.name, theme.css.len()),
                    actual: "missing".into(),
                })
            }
        }
    }
    Ok(())
}

const SCRATCH_THEMES_TABLE: &str = "
CREATE TABLE themes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  org_id INTEGER,
  slug TEXT NOT NULL,
  name TEXT NOT NULL,
  css TEXT NOT NULL,
  usage_count INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
) STRICT;
";
