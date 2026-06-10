use std::path::{Path, PathBuf};

use crate::sql_files::{self, SqlFilesError};

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
}

pub fn seed(database_path: impl AsRef<Path>) -> Result<Vec<String>, SeedError> {
    seed_from(database_path, SEED_DIR)
}

pub fn seed_from(
    database_path: impl AsRef<Path>,
    seeds_dir: impl AsRef<Path>,
) -> Result<Vec<String>, SeedError> {
    sql_files::run(database_path.as_ref(), seeds_dir.as_ref(), None).map_err(map_error)
}

fn map_error(error: SqlFilesError) -> SeedError {
    match error {
        SqlFilesError::DatabaseOpenError { path, source } => {
            SeedError::DatabaseOpenError { path, source }
        }
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
