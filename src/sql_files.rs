use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum SqlFilesError {
    #[error("missing SQL directory: {path}")]
    MissingDirectory { path: PathBuf },

    #[error("SQL path is not a directory: {path}")]
    PathIsNotDirectory { path: PathBuf },

    #[error("no SQL files found in {path}")]
    NoSqlFiles { path: PathBuf },

    #[error("could not read SQL directory {path}: {source}")]
    DirectoryReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not read SQL file {path}: {source}")]
    FileReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid SQL filename: {path}")]
    InvalidFilename { path: PathBuf },

    #[error("duplicate migration number: {first} and {second}")]
    DuplicateNumber { first: PathBuf, second: PathBuf },

    #[error("SQL failed in {path}: {source}")]
    SqlError {
        path: PathBuf,
        source: rusqlite::Error,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SqlFile {
    path: PathBuf,
    version: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum FilenameStyle {
    Numbered,
    Named,
}

pub(crate) fn run_connection(
    conn: &Connection,
    directory: &Path,
    tracking_table: Option<&str>,
    filename_style: FilenameStyle,
) -> Result<Vec<String>, SqlFilesError> {
    let files = discover_files(directory, filename_style)?;

    if let Some(table_name) = tracking_table {
        ensure_tracking_table(conn, table_name)?;
        let applied = read_applied_versions(conn, table_name)?;
        let pending = files
            .into_iter()
            .filter(|file| !applied.contains(&file.version))
            .collect::<Vec<_>>();
        apply_files(conn, pending, tracking_table)
    } else {
        apply_files(conn, files, tracking_table)
    }
}

pub(crate) fn pending_table_rebuild(
    conn: &Connection,
    directory: &Path,
    tracking_table: &str,
) -> Result<bool, SqlFilesError> {
    let files = discover_files(directory, FilenameStyle::Numbered)?;
    let applied = if conn
        .table_exists(Some("main"), tracking_table)
        .map_err(|source| SqlFilesError::SqlError {
            path: directory.to_path_buf(),
            source,
        })? {
        read_applied_versions(conn, tracking_table)?
    } else {
        Vec::new()
    };
    for file in files
        .into_iter()
        .filter(|file| !applied.contains(&file.version))
    {
        if table_rebuild(&read_file(&file)?) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Apply a release batch inside the caller's transaction. Transaction control
/// stays forbidden. Only the owned batch may admit rebuilds, with FK enforcement
/// already disabled before the transaction began.
pub(crate) fn run_in_current_transaction(
    conn: &Connection,
    directory: &Path,
    tracking_table: &str,
    allow_rebuild: bool,
) -> Result<Vec<String>, SqlFilesError> {
    let error = |path, source| SqlFilesError::SqlError { path, source };
    if conn.is_autocommit() {
        return Err(error(
            directory.to_path_buf(),
            rusqlite::Error::InvalidQuery,
        ));
    }
    let foreign_keys: bool = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(|source| error(directory.to_path_buf(), source))?;
    let files = discover_files(directory, FilenameStyle::Numbered)?;
    ensure_tracking_table(conn, tracking_table)?;
    let applied = read_applied_versions(conn, tracking_table)?;
    let mut versions = Vec::new();
    for file in files
        .into_iter()
        .filter(|file| !applied.contains(&file.version))
    {
        let sql = read_file(&file)?;
        if (table_rebuild(&sql) && (!allow_rebuild || foreign_keys))
            || controls_transaction(&sql, true)
                .map_err(|source| error(file.path.clone(), source))?
        {
            return Err(error(file.path, rusqlite::Error::InvalidQuery));
        }
        execute_sql(conn, Some(tracking_table), &file, &sql)?;
        versions.push(file.version);
    }
    Ok(versions)
}

pub(crate) fn discover_files(
    directory: &Path,
    filename_style: FilenameStyle,
) -> Result<Vec<SqlFile>, SqlFilesError> {
    validate_directory(directory)?;

    let mut entries = fs::read_dir(directory)
        .map_err(|source| SqlFilesError::DirectoryReadError {
            path: directory.to_path_buf(),
            source,
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|source| SqlFilesError::DirectoryReadError {
                    path: directory.to_path_buf(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    if entries.is_empty() {
        return Err(SqlFilesError::NoSqlFiles {
            path: directory.to_path_buf(),
        });
    }

    entries.sort();
    let files = entries
        .into_iter()
        .map(|path| sql_file_from_path(path, filename_style))
        .collect::<Result<Vec<_>, _>>()?;
    if matches!(filename_style, FilenameStyle::Numbered) {
        for pair in files.windows(2) {
            if pair[0].version[..3] == pair[1].version[..3] {
                return Err(SqlFilesError::DuplicateNumber {
                    first: pair[0].path.clone(),
                    second: pair[1].path.clone(),
                });
            }
        }
    }
    Ok(files)
}

pub(crate) fn read_versions(
    directory: &Path,
    filename_style: FilenameStyle,
) -> Result<Vec<String>, SqlFilesError> {
    discover_files(directory, filename_style)
        .map(|files| files.into_iter().map(|file| file.version).collect())
}

fn validate_directory(directory: &Path) -> Result<(), SqlFilesError> {
    match fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(SqlFilesError::PathIsNotDirectory {
            path: directory.to_path_buf(),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(SqlFilesError::MissingDirectory {
                path: directory.to_path_buf(),
            })
        }
        Err(source) => Err(SqlFilesError::DirectoryReadError {
            path: directory.to_path_buf(),
            source,
        }),
    }
}

fn sql_file_from_path(
    path: PathBuf,
    filename_style: FilenameStyle,
) -> Result<SqlFile, SqlFilesError> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !valid_filename(filename, filename_style) {
        return Err(SqlFilesError::InvalidFilename { path });
    }

    let version = filename.trim_end_matches(".sql").to_string();
    Ok(SqlFile { path, version })
}

fn valid_filename(filename: &str, filename_style: FilenameStyle) -> bool {
    let Some(stem) = filename.strip_suffix(".sql") else {
        return false;
    };
    let bytes = stem.as_bytes();
    match filename_style {
        FilenameStyle::Numbered => {
            if bytes.len() <= 4 || bytes.get(3) != Some(&b'_') {
                return false;
            }
            bytes[..3].iter().all(u8::is_ascii_digit) && valid_name_bytes(&bytes[4..])
        }
        FilenameStyle::Named => !bytes.is_empty() && valid_name_bytes(bytes),
    }
}

fn valid_name_bytes(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn ensure_tracking_table(conn: &Connection, table_name: &str) -> Result<(), SqlFilesError> {
    conn.execute(
        &format!(
            "create table if not exists {table_name} (
                version text primary key,
                applied_at text not null
            ) strict"
        ),
        [],
    )
    .map(|_| ())
    .map_err(|source| SqlFilesError::SqlError {
        path: PathBuf::from(table_name),
        source,
    })
}

fn read_applied_versions(
    conn: &Connection,
    table_name: &str,
) -> Result<Vec<String>, SqlFilesError> {
    let mut stmt = conn
        .prepare(&format!(
            "select version from {table_name} order by version"
        ))
        .map_err(|source| SqlFilesError::SqlError {
            path: PathBuf::from(table_name),
            source,
        })?;
    stmt.query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| SqlFilesError::SqlError {
            path: PathBuf::from(table_name),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| SqlFilesError::SqlError {
            path: PathBuf::from(table_name),
            source,
        })
}

fn apply_files(
    conn: &Connection,
    files: Vec<SqlFile>,
    tracking_table: Option<&str>,
) -> Result<Vec<String>, SqlFilesError> {
    let mut applied = Vec::new();
    for file in files {
        apply_file(conn, tracking_table, &file)?;
        applied.push(file.version);
    }
    Ok(applied)
}

pub(crate) fn apply_files_in_current_transaction(
    conn: &Connection,
    files: Vec<SqlFile>,
) -> Result<Vec<String>, SqlFilesError> {
    let mut applied = Vec::new();
    for file in files {
        execute_file(conn, None, &file)?;
        applied.push(file.version);
    }
    Ok(applied)
}

fn apply_file(
    conn: &Connection,
    tracking_table: Option<&str>,
    file: &SqlFile,
) -> Result<(), SqlFilesError> {
    let sql = read_file(file)?;
    let rebuild = table_rebuild(&sql);
    let sql_error = |source| SqlFilesError::SqlError {
        path: file.path.clone(),
        source,
    };
    if !conn.is_autocommit() {
        return Err(sql_error(rusqlite::Error::InvalidQuery));
    }
    if rebuild && controls_transaction(&sql, false).map_err(sql_error)? {
        return Err(sql_error(rusqlite::Error::InvalidQuery));
    }
    let foreign_keys: bool = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(sql_error)?;
    if rebuild {
        conn.pragma_update(None, "foreign_keys", false)
            .map_err(sql_error)?;
    }
    let result = (|| {
        conn.execute_batch("begin").map_err(sql_error)?;
        execute_sql(conn, tracking_table, file, &sql)?;
        if rebuild
            && conn
                .prepare("PRAGMA foreign_key_check")
                .map_err(sql_error)?
                .exists([])
                .map_err(sql_error)?
        {
            return Err(sql_error(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                Some("table rebuild leaves invalid foreign keys".to_owned()),
            )));
        }
        conn.execute_batch("commit").map_err(sql_error)
    })();
    let rollback = if result.is_err() && !conn.is_autocommit() {
        conn.execute_batch("rollback").map_err(sql_error)
    } else {
        Ok(())
    };
    let restore = if rebuild {
        conn.pragma_update(None, "foreign_keys", foreign_keys)
            .map_err(sql_error)
    } else {
        Ok(())
    };
    result.and(rollback).and(restore)
}

fn table_rebuild(sql: &str) -> bool {
    sql.lines()
        .any(|line| line.trim().trim_start_matches('\u{feff}').trim() == "-- marmot: rebuild-table")
}

fn controls_transaction(sql: &str, atomic_release: bool) -> rusqlite::Result<bool> {
    use crate::sqlite::tokenize::{Token, tokenize_spans};
    let tokens = tokenize_spans(sql);
    let transaction_statement = |tokens: &[crate::sqlite::tokenize::SpannedToken]| {
        let mut words = tokens.iter().filter_map(|span| match &span.token {
            Token::Word(word) if !word.trim_start_matches('\u{feff}').is_empty() => {
                Some(word.trim_start_matches('\u{feff}'))
            }
            _ => None,
        });
        let Some(first) = words.next() else {
            return false;
        };
        if ["BEGIN", "COMMIT", "END", "ROLLBACK", "SAVEPOINT", "RELEASE"]
            .iter()
            .any(|keyword| first.eq_ignore_ascii_case(keyword))
        {
            return true;
        }
        // Connection settings and attached databases can escape rollback. Only
        // the transaction-local constraint deferral used by migrations is safe.
        atomic_release
            && (["ATTACH", "DETACH", "VACUUM"]
                .iter()
                .any(|keyword| first.eq_ignore_ascii_case(keyword))
                || (first.eq_ignore_ascii_case("PRAGMA")
                    && !words
                        .next()
                        .is_some_and(|name| name.eq_ignore_ascii_case("defer_foreign_keys"))))
    };
    let mut byte_start = 0;
    let mut token_start = 0;
    for (index, span) in tokens.iter().enumerate() {
        if span.token != Token::Semicolon {
            continue;
        }
        let candidate = std::ffi::CString::new(&sql[byte_start..span.end])
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        // SQLite identifies complete statements, including compound trigger bodies.
        // SAFETY: candidate owns a valid NUL-terminated string for this call.
        if unsafe { rusqlite::ffi::sqlite3_complete(candidate.as_ptr()) } != 0 {
            if transaction_statement(&tokens[token_start..=index]) {
                return Ok(true);
            }
            byte_start = span.end;
            token_start = index + 1;
        }
    }
    Ok(transaction_statement(&tokens[token_start..]))
}

fn read_file(file: &SqlFile) -> Result<String, SqlFilesError> {
    fs::read_to_string(&file.path).map_err(|source| SqlFilesError::FileReadError {
        path: file.path.clone(),
        source,
    })
}

fn execute_file(
    conn: &Connection,
    tracking_table: Option<&str>,
    file: &SqlFile,
) -> Result<(), SqlFilesError> {
    let sql = read_file(file)?;
    if table_rebuild(&sql) {
        return Err(SqlFilesError::SqlError {
            path: file.path.clone(),
            source: rusqlite::Error::InvalidQuery,
        });
    }
    execute_sql(conn, tracking_table, file, &sql)
}

fn execute_sql(
    conn: &Connection,
    tracking_table: Option<&str>,
    file: &SqlFile,
    sql: &str,
) -> Result<(), SqlFilesError> {
    conn.execute_batch(sql)
        .map_err(|source| SqlFilesError::SqlError {
            path: file.path.clone(),
            source,
        })?;

    if let Some(table_name) = tracking_table {
        conn.execute(
            &format!("insert into {table_name} (version, applied_at) values (?1, datetime('now'))"),
            [&file.version],
        )
        .map_err(|source| SqlFilesError::SqlError {
            path: file.path.clone(),
            source,
        })?;
    }

    Ok(())
}
