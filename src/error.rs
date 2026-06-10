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

    #[error("invalid -- returns: annotation in {path}: {reason}")]
    InvalidReturnsAnnotation {
        path: PathBuf,
        reason: crate::sqlite::annotation::ReturnsAnnotationError,
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

    #[error("missing SQL directory: {path}")]
    MissingSqlDirectory { path: PathBuf },

    #[error("SQL path is not a directory: {path}")]
    SqlPathNotDirectory { path: PathBuf },

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

    #[error("insert values count mismatch in {path}: expected {expected}, got {got} in row {row}")]
    InsertValuesCountMismatch {
        path: PathBuf,
        expected: usize,
        got: usize,
        row: usize,
    },

    #[error("duplicate result column names in {path}: {columns:?}")]
    DuplicateColumns { path: PathBuf, columns: Vec<String> },

    #[error("generated result column names collide in {path}: {columns:?}")]
    GeneratedColumnNameCollision { path: PathBuf, columns: Vec<String> },

    #[error("shared row type {row_type} has mismatched column shapes")]
    SharedRowTypeMismatch { row_type: String },

    #[error("duplicate generated query names: {names:?}")]
    DuplicateQueryNames { names: Vec<String> },

    #[error("duplicate generated row type names: {names:?}")]
    DuplicateRowTypeNames { names: Vec<String> },

    #[error("anonymous parameters cannot be mixed with named or numbered parameters in {path}")]
    MixedParameterStyles { path: PathBuf },

    #[error("generated file is stale: {path}")]
    StaleGeneratedFile { path: PathBuf },

    #[error("generated output collision: {paths:?}")]
    GeneratedOutputCollision { paths: Vec<PathBuf> },
}

pub type Result<T> = std::result::Result<T, Error>;
