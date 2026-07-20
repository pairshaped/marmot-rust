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
    entries
        .into_iter()
        .map(|path| sql_file_from_path(path, filename_style))
        .collect::<Result<Vec<_>, _>>()
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
    conn.execute_batch("begin")
        .map_err(|source| SqlFilesError::SqlError {
            path: file.path.clone(),
            source,
        })?;

    if let Err(error) = execute_file(conn, tracking_table, file) {
        let _ = conn.execute_batch("rollback");
        return Err(error);
    }

    conn.execute_batch("commit")
        .map_err(|source| SqlFilesError::SqlError {
            path: file.path.clone(),
            source,
        })
}

fn execute_file(
    conn: &Connection,
    tracking_table: Option<&str>,
    file: &SqlFile,
) -> Result<(), SqlFilesError> {
    let sql = fs::read_to_string(&file.path).map_err(|source| SqlFilesError::FileReadError {
        path: file.path.clone(),
        source,
    })?;

    conn.execute_batch(&sql)
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
