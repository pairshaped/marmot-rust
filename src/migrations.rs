use std::path::{Path, PathBuf};

use crate::sql_files::{self, SqlFilesError};

pub const MIGRATION_DIR: &str = "db/migrations";

const TRACKING_TABLE: &str = "schema_migrations";

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
    sql_files::run(
        database_path.as_ref(),
        migrations_dir.as_ref(),
        Some(TRACKING_TABLE),
    )
    .map_err(map_error)
}

fn map_error(error: SqlFilesError) -> MigrationError {
    match error {
        SqlFilesError::DatabaseOpenError { path, source } => {
            MigrationError::DatabaseOpenError { path, source }
        }
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
