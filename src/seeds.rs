use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::sql_files::{self, FilenameStyle, SqlFilesError};

pub const SEED_DIR: &str = "db/seeds";

#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error("could not open SQLite database {path}: {source}")]
    DatabaseOpenError {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("missing seed directory: {path}")]
    MissingSeedDirectory { path: PathBuf },

    #[error("seed path is not a directory: {path}")]
    SeedPathIsNotDirectory { path: PathBuf },

    #[error("no seed files found in {path}")]
    NoSeedFiles { path: PathBuf },

    #[error("could not read seed directory {path}: {source}")]
    SeedDirectoryReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not read seed file {path}: {source}")]
    SeedFileReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid seed filename: {path}")]
    InvalidSeedFilename { path: PathBuf },

    #[error("seed SQL failed in {path}: {source}")]
    SeedSqlError {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("seed data violates foreign keys:\n{violations}")]
    ForeignKeyViolations { violations: String },
}

pub fn seed(database_path: impl AsRef<Path>) -> Result<Vec<String>, SeedError> {
    seed_from(database_path, SEED_DIR)
}

pub fn seed_from(
    database_path: impl AsRef<Path>,
    seeds_dir: impl AsRef<Path>,
) -> Result<Vec<String>, SeedError> {
    let conn = Connection::open(database_path.as_ref()).map_err(|source| {
        SeedError::DatabaseOpenError {
            path: database_path.as_ref().to_path_buf(),
            source,
        }
    })?;
    seed_connection(&conn, seeds_dir)
}

pub fn seed_connection(
    conn: &Connection,
    seeds_dir: impl AsRef<Path>,
) -> Result<Vec<String>, SeedError> {
    seed_directories_connection(conn, [seeds_dir])
}

pub fn seed_directories_from<I, P>(
    database_path: impl AsRef<Path>,
    seed_directories: I,
) -> Result<Vec<String>, SeedError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let conn = Connection::open(database_path.as_ref()).map_err(|source| {
        SeedError::DatabaseOpenError {
            path: database_path.as_ref().to_path_buf(),
            source,
        }
    })?;
    seed_directories_connection(&conn, seed_directories)
}

pub fn seed_directories_connection<I, P>(
    conn: &Connection,
    seed_directories: I,
) -> Result<Vec<String>, SeedError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    conn.pragma_update(None, "foreign_keys", "OFF")
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("foreign_keys"),
            source,
        })?;

    let mut applied = Vec::new();
    for directory in seed_directories {
        applied.extend(
            sql_files::run_connection(conn, directory.as_ref(), None, FilenameStyle::Named)
                .map_err(map_error)?,
        );
    }

    let violations = foreign_key_violations(conn)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("foreign_keys"),
            source,
        })?;
    if violations.is_empty() {
        Ok(applied)
    } else {
        Err(SeedError::ForeignKeyViolations {
            violations: violations.join("\n"),
        })
    }
}

fn foreign_key_violations(conn: &Connection) -> Result<Vec<String>, SeedError> {
    let mut statement =
        conn.prepare("pragma foreign_key_check")
            .map_err(|source| SeedError::SeedSqlError {
                path: PathBuf::from("foreign_key_check"),
                source,
            })?;
    let rows = statement
        .query_map([], |row| {
            Ok(format!(
                "{} rowid {} references {}({})",
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?
            ))
        })
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("foreign_key_check"),
            source,
        })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("foreign_key_check"),
            source,
        })
}

fn map_error(error: SqlFilesError) -> SeedError {
    match error {
        SqlFilesError::MissingDirectory { path } => SeedError::MissingSeedDirectory { path },
        SqlFilesError::PathIsNotDirectory { path } => SeedError::SeedPathIsNotDirectory { path },
        SqlFilesError::NoSqlFiles { path } => SeedError::NoSeedFiles { path },
        SqlFilesError::DirectoryReadError { path, source } => {
            SeedError::SeedDirectoryReadError { path, source }
        }
        SqlFilesError::FileReadError { path, source } => {
            SeedError::SeedFileReadError { path, source }
        }
        SqlFilesError::InvalidFilename { path } => SeedError::InvalidSeedFilename { path },
        SqlFilesError::SqlError { path, source } => SeedError::SeedSqlError { path, source },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;

    use super::*;

    #[test]
    fn runs_seeds_in_filename_order() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            seeds_dir.join("002_add_lucy.sql"),
            "insert into users (id, name) values (1, 'Lucy')",
        )
        .unwrap();
        fs::write(
            seeds_dir.join("001_create_users.sql"),
            "create table users (id integer primary key, name text not null)",
        )
        .unwrap();

        assert_eq!(
            seed_from(&database, &seeds_dir).unwrap(),
            ["001_create_users", "002_add_lucy"]
        );

        let conn = Connection::open(&database).unwrap();
        assert_eq!(user_count(&conn), 1);
    }

    #[test]
    fn accepts_descriptive_seed_names_without_migration_numbers() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            seeds_dir.join("demo.sql"),
            "create table users (id integer primary key, name text not null);
             insert into users (id, name) values (1, 'Lucy')",
        )
        .unwrap();
        fs::write(
            seeds_dir.join("zzz_refresh_counts.sql"),
            "update users set name = name",
        )
        .unwrap();

        assert_eq!(
            seed_from(&database, &seeds_dir).unwrap(),
            ["demo", "zzz_refresh_counts"]
        );
    }

    #[test]
    fn rejects_foreign_key_violations_after_all_seed_directories_run() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&seeds_dir).unwrap();
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "create table parents (id integer primary key);
             create table children (
               id integer primary key,
               parent_id integer not null references parents(id)
             );",
        )
        .unwrap();
        drop(conn);
        fs::write(
            seeds_dir.join("demo.sql"),
            "insert into children (id, parent_id) values (1, 99)",
        )
        .unwrap();

        let error = seed_from(&database, &seeds_dir).unwrap_err().to_string();

        assert!(error.contains("seed data violates foreign keys"));
        assert!(error.contains("children rowid 1 references parents"));
    }

    #[test]
    fn runs_all_seeds_every_time() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            seeds_dir.join("001_create_users.sql"),
            "create table if not exists users (id integer primary key, name text not null)",
        )
        .unwrap();
        fs::write(
            seeds_dir.join("002_add_user.sql"),
            "insert into users (name) values ('Lucy')",
        )
        .unwrap();

        assert_eq!(
            seed_from(&database, &seeds_dir).unwrap(),
            ["001_create_users", "002_add_user"]
        );
        assert_eq!(
            seed_from(&database, &seeds_dir).unwrap(),
            ["001_create_users", "002_add_user"]
        );

        let conn = Connection::open(&database).unwrap();
        assert_eq!(user_count(&conn), 2);
        assert_eq!(table_count(&conn, "schema_seeds"), 0);
    }

    #[test]
    fn rejects_invalid_seed_filenames() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(seeds_dir.join("001-create-users.sql"), "select 1").unwrap();

        assert!(matches!(
            seed_from(&database, &seeds_dir),
            Err(SeedError::InvalidSeedFilename { path })
                if path == seeds_dir.join("001-create-users.sql")
        ));
    }

    #[test]
    fn rejects_missing_seed_directory() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");

        assert!(matches!(
            seed_from(&database, &seeds_dir),
            Err(SeedError::MissingSeedDirectory { path }) if path == seeds_dir
        ));
    }

    #[test]
    fn rejects_seed_path_that_is_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");
        fs::create_dir_all(temp.path().join("db")).unwrap();
        fs::write(&seeds_dir, "not a directory").unwrap();

        assert!(matches!(
            seed_from(&database, &seeds_dir),
            Err(SeedError::SeedPathIsNotDirectory { path }) if path == seeds_dir
        ));
    }

    #[test]
    fn rejects_empty_seed_directory() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&seeds_dir).unwrap();

        assert!(matches!(
            seed_from(&database, &seeds_dir),
            Err(SeedError::NoSeedFiles { path }) if path == seeds_dir
        ));
    }

    #[test]
    fn seed_error_messages_include_context_paths() {
        assert_eq!(
            SeedError::MissingSeedDirectory {
                path: PathBuf::from("db/seeds"),
            }
            .to_string(),
            "missing seed directory: db/seeds"
        );
        assert_eq!(
            SeedError::SeedPathIsNotDirectory {
                path: PathBuf::from("db/seeds"),
            }
            .to_string(),
            "seed path is not a directory: db/seeds"
        );
        assert_eq!(
            SeedError::InvalidSeedFilename {
                path: PathBuf::from("db/seeds/001-create-users.sql"),
            }
            .to_string(),
            "invalid seed filename: db/seeds/001-create-users.sql"
        );
    }

    #[test]
    fn seed_sql_error_message_includes_file_and_sqlite_error() {
        let conn = Connection::open_in_memory().unwrap();
        let source = conn
            .execute_batch("insert into missing_table (id) values (1)")
            .unwrap_err();

        let message = SeedError::SeedSqlError {
            path: PathBuf::from("db/seeds/002_add_lucy.sql"),
            source,
        }
        .to_string();

        assert!(message.contains("seed SQL failed in db/seeds/002_add_lucy.sql"));
        assert!(message.contains("no such table: missing_table"));
    }

    fn user_count(conn: &Connection) -> i64 {
        conn.query_row("select count(*) from users", [], |row| row.get(0))
            .unwrap()
    }

    fn table_count(conn: &Connection, table_name: &str) -> i64 {
        conn.query_row(
            "select count(*) from sqlite_master where type = 'table' and name = ?1",
            [table_name],
            |row| row.get(0),
        )
        .unwrap()
    }
}
