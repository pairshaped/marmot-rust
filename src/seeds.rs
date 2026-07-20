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

    #[error(
        "{seed_error}\nalso failed to restore the caller's foreign-key setting: {restore_error}"
    )]
    ForeignKeyRestoreError {
        seed_error: Box<SeedError>,
        restore_error: rusqlite::Error,
    },
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
    let mut files = Vec::new();
    for directory in seed_directories {
        files.extend(
            sql_files::discover_files(directory.as_ref(), FilenameStyle::Named)
                .map_err(map_error)?,
        );
    }

    let foreign_keys_enabled = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("foreign_keys"),
            source,
        })?;
    conn.pragma_update(None, "foreign_keys", false)
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("foreign_keys"),
            source,
        })?;

    let result = seed_files(conn, files);
    match conn.pragma_update(None, "foreign_keys", foreign_keys_enabled) {
        Ok(()) => result,
        Err(restore_error) => match result {
            Ok(_) => Err(SeedError::SeedSqlError {
                path: PathBuf::from("foreign_keys"),
                source: restore_error,
            }),
            Err(seed_error) => Err(SeedError::ForeignKeyRestoreError {
                seed_error: Box::new(seed_error),
                restore_error,
            }),
        },
    }
}

fn seed_files(conn: &Connection, files: Vec<sql_files::SqlFile>) -> Result<Vec<String>, SeedError> {
    let transaction = conn
        .unchecked_transaction()
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("seed transaction"),
            source,
        })?;

    let applied =
        sql_files::apply_files_in_current_transaction(&transaction, files).map_err(map_error)?;
    let violations = foreign_key_violations(&transaction)?;
    if !violations.is_empty() {
        return Err(SeedError::ForeignKeyViolations {
            violations: violations.join("\n"),
        });
    }

    transaction
        .commit()
        .map_err(|source| SeedError::SeedSqlError {
            path: PathBuf::from("seed transaction"),
            source,
        })?;
    Ok(applied)
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
            let rowid = row
                .get::<_, Option<i64>>(1)?
                .map_or_else(|| "NULL".to_string(), |rowid| rowid.to_string());
            Ok(format!(
                "{} rowid {} references {}({})",
                row.get::<_, String>(0)?,
                rowid,
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

        let conn = Connection::open(&database).unwrap();
        assert_eq!(table_count(&conn, "children"), 1);
        assert_eq!(
            conn.query_row("select count(*) from children", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn rolls_back_earlier_seed_files_when_a_later_file_fails() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            seeds_dir.join("001_add_lucy.sql"),
            "insert into users (id, name) values (1, 'Lucy')",
        )
        .unwrap();
        fs::write(
            seeds_dir.join("002_invalid.sql"),
            "insert into missing_table (id) values (1)",
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "create table users (id integer primary key, name text not null)",
            [],
        )
        .unwrap();

        let error = seed_connection(&conn, &seeds_dir).unwrap_err();

        assert!(matches!(
            error,
            SeedError::SeedSqlError { path, .. }
                if path == seeds_dir.join("002_invalid.sql")
        ));
        assert_eq!(user_count(&conn), 0);
    }

    #[test]
    fn rolls_back_earlier_seed_directories_when_a_later_directory_fails() {
        let temp = tempfile::tempdir().unwrap();
        let bootstrap_dir = temp.path().join("db/bootstrap");
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&bootstrap_dir).unwrap();
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            bootstrap_dir.join("users.sql"),
            "insert into users (id, name) values (1, 'Lucy')",
        )
        .unwrap();
        fs::write(
            seeds_dir.join("invalid.sql"),
            "insert into missing_table (id) values (1)",
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "create table users (id integer primary key, name text not null)",
            [],
        )
        .unwrap();

        let error = seed_directories_connection(&conn, [&bootstrap_dir, &seeds_dir]).unwrap_err();

        assert!(matches!(
            error,
            SeedError::SeedSqlError { path, .. }
                if path == seeds_dir.join("invalid.sql")
        ));
        assert_eq!(user_count(&conn), 0);
    }

    #[test]
    fn restores_enabled_foreign_keys_after_seed_discovery_fails() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("missing");
        let conn = Connection::open_in_memory().unwrap();
        set_foreign_keys(&conn, true);

        assert!(matches!(
            seed_connection(&conn, &seeds_dir),
            Err(SeedError::MissingSeedDirectory { path }) if path == seeds_dir
        ));
        assert!(foreign_keys_enabled(&conn));
    }

    #[test]
    fn restores_enabled_foreign_keys_after_seed_sql_fails() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            seeds_dir.join("invalid.sql"),
            "insert into missing_table (id) values (1)",
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        set_foreign_keys(&conn, true);

        assert!(matches!(
            seed_connection(&conn, &seeds_dir),
            Err(SeedError::SeedSqlError { .. })
        ));
        assert!(foreign_keys_enabled(&conn));
    }

    #[test]
    fn preserves_disabled_foreign_keys_after_successful_seeding() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(seeds_dir.join("valid.sql"), "select 1").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        set_foreign_keys(&conn, false);
        assert!(!foreign_keys_enabled(&conn));

        seed_connection(&conn, &seeds_dir).unwrap();

        assert!(!foreign_keys_enabled(&conn));
    }

    #[test]
    fn preserves_disabled_foreign_keys_after_seed_sql_fails() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            seeds_dir.join("invalid.sql"),
            "insert into missing_table (id) values (1)",
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        set_foreign_keys(&conn, false);

        assert!(matches!(
            seed_connection(&conn, &seeds_dir),
            Err(SeedError::SeedSqlError { .. })
        ));
        assert!(!foreign_keys_enabled(&conn));
    }

    #[test]
    fn restores_foreign_keys_after_a_seed_transaction_cannot_start() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(seeds_dir.join("valid.sql"), "select 1").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        set_foreign_keys(&conn, true);
        conn.execute_batch("begin").unwrap();

        let error = seed_connection(&conn, &seeds_dir).unwrap_err();

        assert!(matches!(
            error,
            SeedError::SeedSqlError { path, .. }
                if path == Path::new("seed transaction")
        ));
        assert!(foreign_keys_enabled(&conn));
        conn.execute_batch("rollback").unwrap();
    }

    #[test]
    fn reports_foreign_key_violations_for_without_rowid_tables() {
        let temp = tempfile::tempdir().unwrap();
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            seeds_dir.join("invalid.sql"),
            "insert into children (id, parent_id) values (1, 99)",
        )
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table parents (id integer primary key);
             create table children (
               id integer primary key,
               parent_id integer not null references parents(id)
             ) without rowid;",
        )
        .unwrap();
        set_foreign_keys(&conn, true);

        let error = seed_connection(&conn, &seeds_dir).unwrap_err().to_string();

        assert!(error.contains("children rowid NULL references parents(0)"));
        assert_eq!(
            conn.query_row("select count(*) from children", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(foreign_keys_enabled(&conn));
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

    fn set_foreign_keys(conn: &Connection, enabled: bool) {
        conn.pragma_update(None, "foreign_keys", enabled).unwrap();
    }

    fn foreign_keys_enabled(conn: &Connection) -> bool {
        conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap()
    }
}
