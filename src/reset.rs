use std::fs;
use std::path::{Path, PathBuf};

use crate::{migrations, seeds};

#[derive(Debug, thiserror::Error)]
pub enum ResetError {
    #[error("database path is a directory: {path}")]
    DatabasePathIsDirectory { path: PathBuf },

    #[error("could not delete SQLite database {path}: {source}")]
    DatabaseDeleteError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error(transparent)]
    MigrationError(#[from] migrations::MigrationError),

    #[error(transparent)]
    SeedError(#[from] seeds::SeedError),
}

pub fn reset(database_path: impl AsRef<Path>) -> Result<(Vec<String>, Vec<String>), ResetError> {
    reset_from(database_path, migrations::MIGRATION_DIR, seeds::SEED_DIR)
}

pub fn reset_from(
    database_path: impl AsRef<Path>,
    migrations_dir: impl AsRef<Path>,
    seeds_dir: impl AsRef<Path>,
) -> Result<(Vec<String>, Vec<String>), ResetError> {
    let database_path = database_path.as_ref();
    drop_database(database_path)?;
    let applied_migrations = migrations::migrate_from(database_path, migrations_dir)?;
    let applied_seeds = seeds::seed_from(database_path, seeds_dir)?;
    Ok((applied_migrations, applied_seeds))
}

fn drop_database(database_path: &Path) -> Result<(), ResetError> {
    match fs::metadata(database_path) {
        Ok(metadata) if metadata.is_dir() => Err(ResetError::DatabasePathIsDirectory {
            path: database_path.to_path_buf(),
        }),
        Ok(_) => drop_database_files(database_path),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            drop_database_files(database_path)
        }
        Err(source) => Err(ResetError::DatabaseDeleteError {
            path: database_path.to_path_buf(),
            source,
        }),
    }
}

fn drop_database_files(database_path: &Path) -> Result<(), ResetError> {
    delete_file_if_present(database_path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        delete_file_if_present(PathBuf::from(format!(
            "{}{suffix}",
            database_path.display()
        )))?;
    }
    Ok(())
}

fn delete_file_if_present(path: impl AsRef<Path>) -> Result<(), ResetError> {
    let path = path.as_ref();
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            fs::remove_file(path).map_err(|source| ResetError::DatabaseDeleteError {
                path: path.to_path_buf(),
                source,
            })
        }
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ResetError::DatabaseDeleteError {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;

    use super::*;

    #[test]
    fn reset_deletes_database_then_runs_migrations_and_seeds() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.db");
        let migrations_dir = temp.path().join("db/migrations");
        let seeds_dir = temp.path().join("db/seeds");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::create_dir_all(&seeds_dir).unwrap();
        fs::write(
            migrations_dir.join("001_create_users.sql"),
            "create table users (id integer primary key, name text not null)",
        )
        .unwrap();
        fs::write(
            seeds_dir.join("001_add_lucy.sql"),
            "insert into users (id, name) values (1, 'Lucy')",
        )
        .unwrap();

        let stale = Connection::open(&database).unwrap();
        stale
            .execute("create table stale (id integer primary key)", [])
            .unwrap();
        drop(stale);

        let (migrations, seeds) = reset_from(&database, &migrations_dir, &seeds_dir).unwrap();

        assert_eq!(migrations, ["001_create_users"]);
        assert_eq!(seeds, ["001_add_lucy"]);
        let conn = Connection::open(&database).unwrap();
        let tables = table_names(&conn);
        assert_eq!(tables, ["schema_migrations", "users"]);
        let name: String = conn
            .query_row("select name from users", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "Lucy");
    }

    #[test]
    fn reset_rejects_database_path_that_is_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.db");
        fs::create_dir_all(&database).unwrap();

        assert!(matches!(
            reset_from(&database, temp.path(), temp.path()),
            Err(ResetError::DatabasePathIsDirectory { path }) if path == database
        ));
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("select name from sqlite_master where type = 'table' order by name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }
}
