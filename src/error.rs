use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not read {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid SQL in {path}: {reason}")]
    InvalidSql {
        path: PathBuf,
        reason: crate::sql_text::SqlValidationError,
    },

    #[error("could not write {path}: {source}")]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not walk {path}: {source}")]
    WalkDir {
        path: PathBuf,
        source: walkdir::Error,
    },

    #[error("could not open sqlite database {path}: {source}")]
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("could not inspect sqlite schema: {source}")]
    InspectDatabase { source: rusqlite::Error },

    #[error("could not prepare SQL in {path}: {source}")]
    PrepareSql {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("duplicate result column names in {path}: {columns:?}")]
    DuplicateColumns { path: PathBuf, columns: Vec<String> },

    #[error("generated result column names collide in {path}: {columns:?}")]
    GeneratedColumnNameCollision { path: PathBuf, columns: Vec<String> },

    #[error("generated file is stale: {path}")]
    StaleGeneratedFile { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, Error>;
