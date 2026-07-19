use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::{migrations, views};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityMode {
    Quick,
    Full,
}

#[derive(Debug, Clone)]
pub struct ValidationConfig {
    pub database: PathBuf,
    pub source_root: PathBuf,
    pub migrations_dir: PathBuf,
    pub migration_table: String,
    pub integrity_mode: IntegrityMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationOutput {
    pub format_version: u32,
    pub databases: Vec<ValidationReport>,
}

impl ValidationOutput {
    pub fn passed(&self) -> bool {
        self.databases.iter().all(ValidationReport::passed)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub database: String,
    pub checks: Vec<ValidationCheck>,
    pub runtime: RuntimeReport,
}

impl ValidationReport {
    pub fn passed(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status == CheckStatus::Passed)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeReport {
    pub sqlite_version: String,
    pub compile_options: Vec<String>,
    pub planner_statistics: PlannerStatistics,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannerStatistics {
    pub present: bool,
    pub rows: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("could not open SQLite database {path} read-only: {source}")]
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("could not inspect SQLite runtime: {0}")]
    Runtime(rusqlite::Error),
}

pub fn validate(config: &ValidationConfig) -> Result<ValidationReport, ValidationError> {
    let connection = Connection::open_with_flags(
        &config.database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| ValidationError::OpenDatabase {
        path: config.database.clone(),
        source,
    })?;

    let checks = vec![
        integrity_check(&connection, config.integrity_mode),
        foreign_key_check(&connection),
        view_check(&connection, &config.source_root),
        migration_check(&connection, &config.migrations_dir, &config.migration_table),
    ];
    let runtime = runtime_report(&connection)?;

    Ok(ValidationReport {
        database: config.database.display().to_string(),
        checks,
        runtime,
    })
}

fn integrity_check(connection: &Connection, mode: IntegrityMode) -> ValidationCheck {
    let (name, pragma) = match mode {
        IntegrityMode::Quick => ("quick_check", "PRAGMA quick_check"),
        IntegrityMode::Full => ("integrity_check", "PRAGMA integrity_check"),
    };
    match string_column(connection, pragma) {
        Ok(rows) if rows == ["ok"] => passed(name),
        Ok(rows) => failed(name, rows),
        Err(error) => failed(name, [error.to_string()]),
    }
}

fn foreign_key_check(connection: &Connection) -> ValidationCheck {
    let result = (|| {
        let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
        let rows = statement
            .query_map([], |row| {
                let table = row.get::<_, String>(0)?;
                let rowid = row.get::<_, Option<i64>>(1)?;
                let parent = row.get::<_, String>(2)?;
                let constraint = row.get::<_, i64>(3)?;
                Ok(format!(
                    "table={table} rowid={} parent={parent} constraint={constraint}",
                    rowid.map_or_else(|| "null".to_string(), |value| value.to_string())
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, rusqlite::Error>(rows)
    })();

    match result {
        Ok(rows) if rows.is_empty() => passed("foreign_keys"),
        Ok(rows) => failed("foreign_keys", rows),
        Err(error) => failed("foreign_keys", [error.to_string()]),
    }
}

fn view_check(connection: &Connection, source_root: &Path) -> ValidationCheck {
    let result = views::discover(source_root)
        .and_then(|definitions| views::audit_connection(connection, &definitions));
    match result {
        Ok(audit) if audit.database_only.is_empty() => passed("views"),
        Ok(audit) => failed(
            "views",
            audit
                .database_only
                .into_iter()
                .map(|name| format!("database-only view: {name}")),
        ),
        Err(error) => failed("views", [error.to_string()]),
    }
}

fn migration_check(
    connection: &Connection,
    migrations_dir: &Path,
    migration_table: &str,
) -> ValidationCheck {
    let expected = match migrations::versions_from(migrations_dir) {
        Ok(versions) => versions.into_iter().collect::<BTreeSet<_>>(),
        Err(error) => return failed("migrations", [error.to_string()]),
    };
    if !valid_identifier(migration_table) {
        return failed(
            "migrations",
            [format!(
                "invalid migration tracking table: {migration_table}"
            )],
        );
    }

    let applied = match string_column(
        connection,
        &format!("SELECT version FROM {migration_table} ORDER BY version"),
    ) {
        Ok(versions) => versions.into_iter().collect::<BTreeSet<_>>(),
        Err(error) => return failed("migrations", [error.to_string()]),
    };
    let mut details = expected
        .difference(&applied)
        .map(|version| format!("pending migration: {version}"))
        .collect::<Vec<_>>();
    details.extend(
        applied
            .difference(&expected)
            .map(|version| format!("untracked migration: {version}")),
    );

    if details.is_empty() {
        passed("migrations")
    } else {
        failed("migrations", details)
    }
}

fn runtime_report(connection: &Connection) -> Result<RuntimeReport, ValidationError> {
    let sqlite_version = connection
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .map_err(ValidationError::Runtime)?;
    let compile_options =
        string_column(connection, "PRAGMA compile_options").map_err(ValidationError::Runtime)?;
    let statistics_present = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema
                WHERE type = 'table' AND name = 'sqlite_stat1'
            )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(ValidationError::Runtime)?;
    let statistics_rows = if statistics_present {
        connection
            .query_row("SELECT COUNT(*) FROM sqlite_stat1", [], |row| row.get(0))
            .map_err(ValidationError::Runtime)?
    } else {
        0
    };

    Ok(RuntimeReport {
        sqlite_version,
        compile_options,
        planner_statistics: PlannerStatistics {
            present: statistics_present,
            rows: statistics_rows,
        },
    })
}

fn string_column(connection: &Connection, sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(sql)?;
    statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()
}

fn valid_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn passed(name: &'static str) -> ValidationCheck {
    ValidationCheck {
        name,
        status: CheckStatus::Passed,
        details: Vec::new(),
    }
}

fn failed(name: &'static str, details: impl IntoIterator<Item = String>) -> ValidationCheck {
    ValidationCheck {
        name,
        status: CheckStatus::Failed,
        details: details.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};

    use super::*;

    fn fixture() -> (tempfile::TempDir, ValidationConfig) {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("app.db");
        let migrations_dir = temporary.path().join("db/migrations");
        let source_root = temporary.path().join("src");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::create_dir_all(&source_root).unwrap();
        fs::write(
            migrations_dir.join("001_create_users.sql"),
            "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE users (id INTEGER PRIMARY KEY);
                 CREATE TABLE registrations (
                   id INTEGER PRIMARY KEY,
                   user_id INTEGER NOT NULL REFERENCES users(id)
                 );
                 CREATE TABLE schema_versions (
                   version TEXT PRIMARY KEY,
                   applied_at TEXT NOT NULL
                 ) STRICT;
                 INSERT INTO schema_versions VALUES ('001_create_users', datetime('now'));",
            )
            .unwrap();

        (
            temporary,
            ValidationConfig {
                database,
                source_root,
                migrations_dir,
                migration_table: "schema_versions".to_string(),
                integrity_mode: IntegrityMode::Quick,
            },
        )
    }

    #[test]
    fn validates_a_healthy_database() {
        let (_temporary, config) = fixture();

        let report = validate(&config).unwrap();

        assert!(report.passed());
        assert!(!report.runtime.sqlite_version.is_empty());
        assert!(!report.runtime.compile_options.is_empty());
        assert!(!report.runtime.planner_statistics.present);
    }

    #[test]
    fn reports_foreign_key_violations() {
        let (_temporary, config) = fixture();
        let connection = Connection::open(&config.database).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO registrations (id, user_id) VALUES (1, 99);",
            )
            .unwrap();

        let report = validate(&config).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "foreign_keys")
            .unwrap();

        assert_eq!(check.status, CheckStatus::Failed);
        assert!(check.details[0].contains("table=registrations"));
    }

    #[test]
    fn reports_pending_and_untracked_migrations() {
        let (_temporary, config) = fixture();
        fs::write(
            config.migrations_dir.join("002_add_email.sql"),
            "ALTER TABLE users ADD COLUMN email TEXT;",
        )
        .unwrap();
        let connection = Connection::open(&config.database).unwrap();
        connection
            .execute(
                "INSERT INTO schema_versions VALUES ('999_unknown', datetime('now'))",
                [],
            )
            .unwrap();

        let report = validate(&config).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "migrations")
            .unwrap();

        assert_eq!(check.status, CheckStatus::Failed);
        assert!(
            check
                .details
                .contains(&"pending migration: 002_add_email".to_string())
        );
        assert!(
            check
                .details
                .contains(&"untracked migration: 999_unknown".to_string())
        );
    }

    #[test]
    fn reports_declared_views_missing_from_the_database() {
        let (_temporary, config) = fixture();
        let view_directory = config.source_root.join(views::VIEW_DIR);
        fs::create_dir_all(&view_directory).unwrap();
        fs::write(
            view_directory.join("view_users.sql"),
            "CREATE VIEW view_users (id) AS SELECT id FROM users;",
        )
        .unwrap();

        let report = validate(&config).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "views")
            .unwrap();

        assert_eq!(check.status, CheckStatus::Failed);
        assert!(check.details[0].contains("missing from the database"));
    }

    #[test]
    fn full_integrity_check_reports_reproducible_page_corruption() {
        let temporary = tempfile::tempdir().unwrap();
        let database = temporary.path().join("corrupt.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA page_size = 4096;
                 CREATE TABLE payloads (id INTEGER PRIMARY KEY, value BLOB NOT NULL);
                 INSERT INTO payloads (value) VALUES (zeroblob(12000));",
            )
            .unwrap();
        drop(connection);

        let mut file = fs::OpenOptions::new().write(true).open(&database).unwrap();
        file.seek(SeekFrom::Start(4096)).unwrap();
        file.write_all(&[0; 256]).unwrap();
        drop(file);

        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let check = integrity_check(&connection, IntegrityMode::Full);

        assert_eq!(check.status, CheckStatus::Failed);
        assert!(!check.details.is_empty());
    }
}
