use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::sql_files::{self, FilenameStyle, SqlFilesError};

pub const MIGRATION_DIR: &str = "db/migrations";

pub const TRACKING_TABLE: &str = "schema_migrations";

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("could not open SQLite database {path}: {source}")]
    DatabaseOpenError {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("missing migration directory: {path}")]
    MissingMigrationDirectory { path: PathBuf },

    #[error("migration path is not a directory: {path}")]
    MigrationPathIsNotDirectory { path: PathBuf },

    #[error("no migration files found in {path}")]
    NoMigrationFiles { path: PathBuf },

    #[error("could not read migration directory {path}: {source}")]
    MigrationDirectoryReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not read migration file {path}: {source}")]
    MigrationFileReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid migration filename: {path}")]
    InvalidMigrationFilename { path: PathBuf },

    #[error("invalid migration tracking table name: {name}")]
    InvalidTrackingTable { name: String },

    #[error("migration SQL failed in {path}: {source}")]
    MigrationSqlError {
        path: PathBuf,
        source: rusqlite::Error,
    },
}

pub fn migrate(database_path: impl AsRef<Path>) -> Result<Vec<String>, MigrationError> {
    migrate_from(database_path, MIGRATION_DIR)
}

pub fn migrate_from(
    database_path: impl AsRef<Path>,
    migrations_dir: impl AsRef<Path>,
) -> Result<Vec<String>, MigrationError> {
    migrate_from_with_tracking_table(database_path, migrations_dir, TRACKING_TABLE)
}

pub fn migrate_from_with_tracking_table(
    database_path: impl AsRef<Path>,
    migrations_dir: impl AsRef<Path>,
    tracking_table: &str,
) -> Result<Vec<String>, MigrationError> {
    let conn = Connection::open(database_path.as_ref()).map_err(|source| {
        MigrationError::DatabaseOpenError {
            path: database_path.as_ref().to_path_buf(),
            source,
        }
    })?;
    migrate_connection(&conn, migrations_dir, tracking_table)
}

pub fn versions_from(migrations_dir: impl AsRef<Path>) -> Result<Vec<String>, MigrationError> {
    sql_files::read_versions(migrations_dir.as_ref(), FilenameStyle::Numbered).map_err(map_error)
}

pub fn migrate_connection(
    conn: &Connection,
    migrations_dir: impl AsRef<Path>,
    tracking_table: &str,
) -> Result<Vec<String>, MigrationError> {
    validate_tracking_table(tracking_table)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|source| MigrationError::MigrationSqlError {
            path: migrations_dir.as_ref().to_path_buf(),
            source,
        })?;
    sql_files::run_connection(
        conn,
        migrations_dir.as_ref(),
        Some(tracking_table),
        FilenameStyle::Numbered,
    )
    .map_err(map_error)
}

fn validate_tracking_table(name: &str) -> Result<(), MigrationError> {
    let mut bytes = name.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_');
    if valid_first
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Ok(())
    } else {
        Err(MigrationError::InvalidTrackingTable {
            name: name.to_string(),
        })
    }
}

fn map_error(error: SqlFilesError) -> MigrationError {
    match error {
        SqlFilesError::MissingDirectory { path } => {
            MigrationError::MissingMigrationDirectory { path }
        }
        SqlFilesError::PathIsNotDirectory { path } => {
            MigrationError::MigrationPathIsNotDirectory { path }
        }
        SqlFilesError::NoSqlFiles { path } => MigrationError::NoMigrationFiles { path },
        SqlFilesError::DirectoryReadError { path, source } => {
            MigrationError::MigrationDirectoryReadError { path, source }
        }
        SqlFilesError::FileReadError { path, source } => {
            MigrationError::MigrationFileReadError { path, source }
        }
        SqlFilesError::InvalidFilename { path } => {
            MigrationError::InvalidMigrationFilename { path }
        }
        SqlFilesError::SqlError { path, source } => {
            MigrationError::MigrationSqlError { path, source }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;

    use super::*;

    #[test]
    fn applies_migrations_in_filename_order() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(
            migrations_dir.join("002_add_email.sql"),
            "alter table users add column email text",
        )
        .unwrap();
        fs::write(
            migrations_dir.join("001_create_users.sql"),
            "create table users (id integer primary key)",
        )
        .unwrap();

        let applied = migrate_from(&database, &migrations_dir).unwrap();

        assert_eq!(applied, ["001_create_users", "002_add_email"]);
        let conn = Connection::open(&database).unwrap();
        assert_eq!(
            applied_versions(&conn),
            ["001_create_users", "002_add_email"]
        );
        conn.execute(
            "insert into users (id, email) values (1, 'lucy@example.com')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn skips_already_applied_migrations() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(
            migrations_dir.join("001_create_users.sql"),
            "create table users (id integer primary key)",
        )
        .unwrap();

        assert_eq!(
            migrate_from(&database, &migrations_dir).unwrap(),
            ["001_create_users"]
        );
        assert_eq!(
            migrate_from(&database, &migrations_dir).unwrap(),
            Vec::<String>::new()
        );

        let conn = Connection::open(&database).unwrap();
        assert_eq!(applied_versions(&conn), ["001_create_users"]);
    }

    #[test]
    fn failed_migration_is_not_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(
            migrations_dir.join("001_create_users.sql"),
            "create table users (id integer primary key)",
        )
        .unwrap();
        fs::write(
            migrations_dir.join("002_insert_missing.sql"),
            "insert into missing_table (id) values (1)",
        )
        .unwrap();

        assert!(matches!(
            migrate_from(&database, &migrations_dir),
            Err(MigrationError::MigrationSqlError { .. })
        ));
        let conn = Connection::open(&database).unwrap();
        assert_eq!(applied_versions(&conn), ["001_create_users"]);
    }

    #[test]
    fn rejects_invalid_migration_filenames() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(migrations_dir.join("002-add-email.sql"), "select 1").unwrap();

        assert!(matches!(
            migrate_from(&database, &migrations_dir),
            Err(MigrationError::InvalidMigrationFilename { path })
                if path == migrations_dir.join("002-add-email.sql")
        ));
    }

    #[test]
    fn supports_a_project_specific_tracking_table() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(
            migrations_dir.join("001_create_users.sql"),
            "create table users (id integer primary key)",
        )
        .unwrap();

        migrate_from_with_tracking_table(&database, &migrations_dir, "schema_versions").unwrap();

        let conn = Connection::open(&database).unwrap();
        let version: String = conn
            .query_row("select version from schema_versions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, "001_create_users");
    }

    #[test]
    fn rejects_unsafe_tracking_table_names() {
        let conn = Connection::open_in_memory().unwrap();

        assert!(matches!(
            migrate_connection(&conn, "db/migrations", "versions; drop table users"),
            Err(MigrationError::InvalidTrackingTable { .. })
        ));
    }

    #[test]
    fn rejects_missing_migration_directory() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");

        assert!(matches!(
            migrate_from(&database, &migrations_dir),
            Err(MigrationError::MissingMigrationDirectory { path }) if path == migrations_dir
        ));
    }

    #[test]
    fn rejects_migration_path_that_is_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(temp.path().join("db")).unwrap();
        fs::write(&migrations_dir, "not a directory").unwrap();

        assert!(matches!(
            migrate_from(&database, &migrations_dir),
            Err(MigrationError::MigrationPathIsNotDirectory { path }) if path == migrations_dir
        ));
    }

    #[test]
    fn rejects_empty_migration_directory() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&migrations_dir).unwrap();

        assert!(matches!(
            migrate_from(&database, &migrations_dir),
            Err(MigrationError::NoMigrationFiles { path }) if path == migrations_dir
        ));
    }

    #[test]
    fn migration_error_messages_include_context_paths() {
        assert_eq!(
            MigrationError::MissingMigrationDirectory {
                path: PathBuf::from("db/migrations"),
            }
            .to_string(),
            "missing migration directory: db/migrations"
        );
        assert_eq!(
            MigrationError::MigrationPathIsNotDirectory {
                path: PathBuf::from("db/migrations"),
            }
            .to_string(),
            "migration path is not a directory: db/migrations"
        );
        assert_eq!(
            MigrationError::InvalidMigrationFilename {
                path: PathBuf::from("db/migrations/001-create-users.sql"),
            }
            .to_string(),
            "invalid migration filename: db/migrations/001-create-users.sql"
        );
    }

    #[test]
    fn migration_sql_error_message_includes_file_and_sqlite_error() {
        let conn = Connection::open_in_memory().unwrap();
        let source = conn
            .execute_batch("insert into missing_table (id) values (1)")
            .unwrap_err();

        let message = MigrationError::MigrationSqlError {
            path: PathBuf::from("db/migrations/002_insert_missing.sql"),
            source,
        }
        .to_string();

        assert!(message.contains("migration SQL failed in db/migrations/002_insert_missing.sql"));
        assert!(message.contains("no such table: missing_table"));
    }

    fn applied_versions(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("select version from schema_migrations order by version")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }
}
