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

    InvalidSqlBlock {
        path: PathBuf,
        reason: crate::sqlite::blocks::SqlBlockError,
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

    SqlDirectoryQueryFile {
        path: PathBuf,
    },

    ModSqlFile {
        path: PathBuf,
    },

    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },

    ReadInitSql {
        path: PathBuf,
        source: std::io::Error,
    },

    RunInitSql {
        path: PathBuf,
        source: rusqlite::Error,
    },

    InspectDatabase {
        source: rusqlite::Error,
    },

    TemporalColumnTypeMismatch {
        table: String,
        column: String,
        declared_type: String,
        expected: &'static str,
    },

    ConflictingTemporalParameterTypes {
        path: PathBuf,
        parameter: String,
        first: &'static str,
        second: &'static str,
    },

    InvalidBooleanConstraint {
        table: String,
        column: String,
        reason: String,
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

    DuplicateQueryNames {
        names: Vec<String>,
    },

    DuplicateRowTypeNames {
        names: Vec<String>,
    },

    GeneratedTemporalModuleCollision,

    MixedParameterStyles {
        path: PathBuf,
    },

    StaleGeneratedFile {
        path: PathBuf,
    },

    GeneratedOutputCollision {
        paths: Vec<PathBuf>,
    },

    View {
        source: crate::views::ViewError,
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
            Self::InvalidSqlBlock { path, reason } => {
                write!(f, "invalid SQL block in {}: {reason}", path.display())
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
            Self::SqlDirectoryQueryFile { path } => {
                write!(
                    f,
                    "SQL query files under directories named `sql` are no longer supported: {}. Use a Rust module companion SQL file with `-- func:` blocks.",
                    path.display()
                )
            }
            Self::ModSqlFile { path } => {
                write!(
                    f,
                    "mod.sql is not supported: {}. Put SQL beside the Rust file that owns it, such as index.sql, form.sql, or common.sql.",
                    path.display()
                )
            }
            Self::OpenDatabase { path, source } => {
                write!(
                    f,
                    "could not open sqlite database {}: {source}",
                    path.display()
                )
            }
            Self::ReadInitSql { path, source } => {
                write!(f, "could not read init_sql {}: {source}", path.display())
            }
            Self::RunInitSql { path, source } => {
                write!(f, "could not run init_sql {}: {source}", path.display())
            }
            Self::InspectDatabase { source } => {
                write!(f, "could not inspect sqlite schema: {source}")
            }
            Self::TemporalColumnTypeMismatch {
                table,
                column,
                declared_type,
                expected,
            } => write!(
                f,
                "temporal column {table}.{column} must be declared as {expected}, got {declared_type:?}"
            ),
            Self::ConflictingTemporalParameterTypes {
                path,
                parameter,
                first,
                second,
            } => write!(
                f,
                "parameter {parameter} has conflicting temporal types {first} and {second} in {}",
                path.display()
            ),
            Self::InvalidBooleanConstraint {
                table,
                column,
                reason,
            } => write!(
                f,
                "invalid boolean constraint on {table}.{column}: {reason}"
            ),
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
            Self::DuplicateQueryNames { names } => {
                write!(f, "duplicate generated query names: {names:?}")
            }
            Self::DuplicateRowTypeNames { names } => {
                write!(f, "duplicate generated row type names: {names:?}")
            }
            Self::GeneratedTemporalModuleCollision => write!(
                f,
                "temporal types require a generated temporal module, but the project already has a root temporal SQL module"
            ),
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
            Self::View { source } => source.fmt(f),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadFile { source, .. }
            | Self::WriteFile { source, .. }
            | Self::CreateDir { source, .. }
            | Self::ReadInitSql { source, .. } => Some(source),
            Self::WalkDir { source, .. } => Some(source),
            Self::OpenDatabase { source, .. }
            | Self::InspectDatabase { source }
            | Self::RunInitSql { source, .. }
            | Self::PrepareSql { source, .. } => Some(source),
            Self::View { source } => Some(source),
            _ => None,
        }
    }
}

impl From<crate::views::ViewError> for Error {
    fn from(source: crate::views::ViewError) -> Self {
        Self::View { source }
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
