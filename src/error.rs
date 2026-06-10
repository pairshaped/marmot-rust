use std::path::PathBuf;

#[derive(Debug)]
pub enum Error {
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    InvalidSql {
        path: PathBuf,
        reason: crate::sql_text::SqlValidationError,
    },

    InvalidReturnsAnnotation {
        path: PathBuf,
        reason: crate::sqlite::annotation::ReturnsAnnotationError,
    },

    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },

    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },

    WalkDir {
        path: PathBuf,
        source: walkdir::Error,
    },

    MissingSqlDirectory {
        path: PathBuf,
    },

    SqlPathNotDirectory {
        path: PathBuf,
    },

    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },

    InspectDatabase {
        source: rusqlite::Error,
    },

    PrepareSql {
        path: PathBuf,
        source: rusqlite::Error,
    },

    InsertValuesCountMismatch {
        path: PathBuf,
        expected: usize,
        got: usize,
        row: usize,
    },

    DuplicateColumns {
        path: PathBuf,
        columns: Vec<String>,
    },

    GeneratedColumnNameCollision {
        path: PathBuf,
        columns: Vec<String>,
    },

    SharedRowTypeMismatch {
        row_type: String,
    },

    DuplicateQueryNames {
        names: Vec<String>,
    },

    DuplicateRowTypeNames {
        names: Vec<String>,
    },

    MixedParameterStyles {
        path: PathBuf,
    },

    StaleGeneratedFile {
        path: PathBuf,
    },

    GeneratedOutputCollision {
        paths: Vec<PathBuf>,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadFile { path, source } => {
                write!(f, "could not read {}: {source}", path.display())
            }
            Self::InvalidSql { path, reason } => {
                write!(f, "invalid SQL in {}: {reason}", path.display())
            }
            Self::InvalidReturnsAnnotation { path, reason } => {
                write!(
                    f,
                    "invalid -- returns: annotation in {}: {reason}",
                    path.display()
                )
            }
            Self::WriteFile { path, source } => {
                write!(f, "could not write {}: {source}", path.display())
            }
            Self::CreateDir { path, source } => {
                write!(f, "could not create directory {}: {source}", path.display())
            }
            Self::WalkDir { path, source } => {
                write!(f, "could not walk {}: {source}", path.display())
            }
            Self::MissingSqlDirectory { path } => {
                write!(f, "missing SQL directory: {}", path.display())
            }
            Self::SqlPathNotDirectory { path } => {
                write!(f, "SQL path is not a directory: {}", path.display())
            }
            Self::OpenDatabase { path, source } => {
                write!(
                    f,
                    "could not open sqlite database {}: {source}",
                    path.display()
                )
            }
            Self::InspectDatabase { source } => {
                write!(f, "could not inspect sqlite schema: {source}")
            }
            Self::PrepareSql { path, source } => {
                write!(f, "could not prepare SQL in {}: {source}", path.display())?;
                if let Some(hint) = sql_error_hint(&source.to_string()) {
                    write!(f, "\n{hint}")?;
                }
                Ok(())
            }
            Self::InsertValuesCountMismatch {
                path,
                expected,
                got,
                row,
            } => write!(
                f,
                "insert values count mismatch in {}: expected {expected}, got {got} in row {row}",
                path.display()
            ),
            Self::DuplicateColumns { path, columns } => {
                write!(
                    f,
                    "duplicate result column names in {}: {columns:?}",
                    path.display()
                )
            }
            Self::GeneratedColumnNameCollision { path, columns } => {
                write!(
                    f,
                    "generated result column names collide in {}: {columns:?}",
                    path.display()
                )
            }
            Self::SharedRowTypeMismatch { row_type } => {
                write!(f, "shared row type {row_type} has mismatched column shapes")
            }
            Self::DuplicateQueryNames { names } => {
                write!(f, "duplicate generated query names: {names:?}")
            }
            Self::DuplicateRowTypeNames { names } => {
                write!(f, "duplicate generated row type names: {names:?}")
            }
            Self::MixedParameterStyles { path } => {
                write!(
                    f,
                    "anonymous parameters cannot be mixed with named or numbered parameters in {}",
                    path.display()
                )
            }
            Self::StaleGeneratedFile { path } => {
                write!(f, "generated file is stale: {}", path.display())
            }
            Self::GeneratedOutputCollision { paths } => {
                write!(f, "generated output collision: {paths:?}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadFile { source, .. }
            | Self::WriteFile { source, .. }
            | Self::CreateDir { source, .. } => Some(source),
            Self::WalkDir { source, .. } => Some(source),
            Self::OpenDatabase { source, .. }
            | Self::InspectDatabase { source }
            | Self::PrepareSql { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn sql_error_hint(message: &str) -> Option<&'static str> {
    if message.contains("row value misused") {
        return Some(
            "hint: Did you accidentally parenthesize your SELECT columns?\n\
             Write: SELECT id, name FROM ...\n\
             Not:   SELECT (id, name) FROM ...",
        );
    }
    if message.contains("no such table") {
        return Some(
            "hint: Make sure the database file contains your schema.\n\
             Marmot needs the tables to exist so it can infer types.",
        );
    }
    if message.contains("no such column") {
        return Some(
            "hint: Check that the column name matches your schema exactly.\n\
             Column names are case-sensitive in some contexts.",
        );
    }
    None
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn prepare_sql_error_adds_no_such_table_hint() {
        let conn = Connection::open_in_memory().unwrap();
        let source = conn.prepare("select * from users").unwrap_err();

        let message = Error::PrepareSql {
            path: PathBuf::from("src/app/sql/list_users.sql"),
            source,
        }
        .to_string();

        assert!(message.contains("no such table: users"));
        assert!(message.contains("hint: Make sure the database file contains your schema."));
        assert!(message.contains("Marmot needs the tables to exist so it can infer types."));
    }

    #[test]
    fn prepare_sql_error_adds_no_such_column_hint() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("create table users (id integer primary key);")
            .unwrap();
        let source = conn.prepare("select email from users").unwrap_err();

        let message = Error::PrepareSql {
            path: PathBuf::from("src/app/sql/list_users.sql"),
            source,
        }
        .to_string();

        assert!(message.contains("no such column: email"));
        assert!(message.contains("hint: Check that the column name matches your schema exactly."));
        assert!(message.contains("Column names are case-sensitive in some contexts."));
    }

    #[test]
    fn prepare_sql_error_adds_row_value_hint() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("create table users (id integer primary key, name text);")
            .unwrap();
        let source = conn.prepare("select (id, name) from users").unwrap_err();

        let message = Error::PrepareSql {
            path: PathBuf::from("src/app/sql/list_users.sql"),
            source,
        }
        .to_string();

        assert!(message.contains("row value misused"));
        assert!(message.contains("hint: Did you accidentally parenthesize your SELECT columns?"));
        assert!(message.contains("Write: SELECT id, name FROM ..."));
        assert!(message.contains("Not:   SELECT (id, name) FROM ..."));
    }
}
