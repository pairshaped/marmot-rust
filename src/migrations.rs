use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::maintenance::{self, OptimizeScope};
use crate::sql_files::{self, FilenameStyle, SqlFilesError};

pub const MIGRATION_DIR: &str = "db/migrations";

pub const TRACKING_TABLE: &str = "schema_migrations";

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("could not read migration scalar declarations from {path}: {source}")]
    InitSqlReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not register migration scalar declarations from {path}: {source}")]
    InitSqlDeclarationsError {
        path: PathBuf,
        source: Box<crate::Error>,
    },

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

    #[error("duplicate migration number: {first} and {second}")]
    DuplicateNumber { first: PathBuf, second: PathBuf },

    #[error("invalid migration filename: {path}")]
    InvalidMigrationFilename { path: PathBuf },

    #[error("invalid migration tracking table name: {name}")]
    InvalidTrackingTable { name: String },

    #[error("migration SQL failed in {path}: {source}")]
    MigrationSqlError {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("could not optimize SQLite planner statistics after migrations in {path}: {source}")]
    OptimizationError {
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
    migrate_from_with_tracking_table_and_init_sql(
        database_path,
        migrations_dir,
        tracking_table,
        None,
    )
}

pub fn migrate_from_with_tracking_table_and_init_sql(
    database_path: impl AsRef<Path>,
    migrations_dir: impl AsRef<Path>,
    tracking_table: &str,
    init_sql: Option<&Path>,
) -> Result<Vec<String>, MigrationError> {
    let conn = Connection::open(database_path.as_ref()).map_err(|source| {
        MigrationError::DatabaseOpenError {
            path: database_path.as_ref().to_path_buf(),
            source,
        }
    })?;
    migrate_connection_with_init_sql(&conn, migrations_dir, tracking_table, init_sql)
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
    let applied = sql_files::run_connection(
        conn,
        migrations_dir.as_ref(),
        Some(tracking_table),
        FilenameStyle::Numbered,
    )
    .map_err(map_error)?;
    if !applied.is_empty() {
        maintenance::optimize(conn, OptimizeScope::SchemaChange).map_err(|source| {
            MigrationError::OptimizationError {
                path: migrations_dir.as_ref().to_path_buf(),
                source,
            }
        })?;
    }
    Ok(applied)
}

/// Apply pending versions without committing. The caller owns rollback, view
/// reconciliation and validation, and must enable foreign keys before beginning.
/// Rebuild directives that need foreign_keys=OFF are rejected in this mode.
pub fn migrate_connection_in_transaction(
    conn: &Connection,
    migrations_dir: impl AsRef<Path>,
    tracking_table: &str,
) -> Result<Vec<String>, MigrationError> {
    validate_tracking_table(tracking_table)?;
    let applied =
        sql_files::run_in_current_transaction(conn, migrations_dir.as_ref(), tracking_table, false)
            .map_err(map_error)?;
    if !applied.is_empty() {
        maintenance::optimize(conn, OptimizeScope::SchemaChange).map_err(|source| {
            MigrationError::OptimizationError {
                path: migrations_dir.as_ref().to_path_buf(),
                source,
            }
        })?;
    }
    Ok(applied)
}

/// Apply and validate one release batch, including referenced-table rebuilds.
/// The connection must be outside a transaction. Pending rebuilds disable foreign
/// keys before BEGIN; all versions and caller validation share that transaction.
/// Dry runs and errors roll back the whole batch. Foreign keys are ON on return.
pub fn migrate_connection_atomically<E>(
    conn: &mut Connection,
    migrations_dir: impl AsRef<Path>,
    tracking_table: &str,
    dry_run: bool,
    validate: impl FnOnce(&Connection) -> Result<(), E>,
) -> Result<Vec<String>, E>
where
    E: From<MigrationError>,
{
    let directory = migrations_dir.as_ref();
    let sql_error = |source| MigrationError::MigrationSqlError {
        path: directory.to_path_buf(),
        source,
    };
    if !conn.is_autocommit() {
        return Err(sql_error(rusqlite::Error::InvalidQuery).into());
    }
    let result = (|| -> Result<Vec<String>, E> {
        validate_tracking_table(tracking_table)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        let rebuild =
            sql_files::pending_table_rebuild(conn, directory, tracking_table).map_err(map_error)?;
        if rebuild {
            conn.pragma_update(None, "foreign_keys", false)
                .map_err(sql_error)?;
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let applied =
            sql_files::run_in_current_transaction(&tx, directory, tracking_table, rebuild)
                .map_err(map_error)?;
        if !applied.is_empty() {
            maintenance::optimize(&tx, OptimizeScope::SchemaChange).map_err(|source| {
                MigrationError::OptimizationError {
                    path: directory.to_path_buf(),
                    source,
                }
            })?;
        }
        validate(&tx)?;
        if rebuild
            && tx
                .prepare("PRAGMA foreign_key_check")
                .map_err(sql_error)?
                .exists([])
                .map_err(sql_error)?
        {
            return Err(sql_error(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
                Some("release batch leaves invalid foreign keys".into()),
            ))
            .into());
        }
        if dry_run {
            tx.rollback().map_err(sql_error)?;
        } else {
            tx.commit().map_err(sql_error)?;
        }
        Ok(applied)
    })();
    // Any failed transaction has dropped and rolled back before restoring this
    // connection setting, which SQLite cannot change inside a transaction.
    let restored = conn
        .pragma_update(None, "foreign_keys", true)
        .map_err(sql_error)
        .map_err(E::from);
    match (result, restored) {
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(applied), Ok(())) => Ok(applied),
    }
}

/// Register declared scalars for schema validation, without executing initialization SQL.
/// These placeholders refuse evaluation; callers with real application functions should
/// use `migrate_connection` instead.
pub fn migrate_connection_with_init_sql(
    conn: &Connection,
    migrations_dir: impl AsRef<Path>,
    tracking_table: &str,
    init_sql: Option<&Path>,
) -> Result<Vec<String>, MigrationError> {
    if let Some(path) = init_sql {
        let sql =
            std::fs::read_to_string(path).map_err(|source| MigrationError::InitSqlReadError {
                path: path.to_path_buf(),
                source,
            })?;
        crate::analyzer::register_analysis_functions(conn, path, &sql).map_err(|source| {
            MigrationError::InitSqlDeclarationsError {
                path: path.to_path_buf(),
                source: Box::new(source),
            }
        })?;
    }
    migrate_connection(conn, migrations_dir, tracking_table)
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
        SqlFilesError::DuplicateNumber { first, second } => {
            MigrationError::DuplicateNumber { first, second }
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
    fn discovery_rejects_duplicate_numbers_and_names_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("140_first.sql");
        let second = dir.path().join("140_second.sql");
        fs::write(&first, "not SQL").unwrap();
        fs::write(&second, "not SQL").unwrap();
        let error = versions_from(dir.path()).unwrap_err();
        assert!(error.to_string().contains("140_first.sql"));
        assert!(error.to_string().contains("140_second.sql"));
        assert!(
            matches!(error, MigrationError::DuplicateNumber { first: a, second: b }
            if a == first && b == second)
        );
    }

    #[test]
    fn discovery_preserves_distinct_full_versions_in_number_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["141_next.sql", "139_previous.sql", "140_current.sql"] {
            fs::write(dir.path().join(name), "not SQL").unwrap();
        }
        assert_eq!(
            versions_from(dir.path()).unwrap(),
            ["139_previous", "140_current", "141_next"]
        );
    }

    #[test]
    fn forward_migration_validates_views_with_declared_application_scalars() {
        let temp = tempfile::tempdir().unwrap();
        let migrations = temp.path().join("migrations");
        fs::create_dir(&migrations).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users (email TEXT, obsolete TEXT);
             INSERT INTO users VALUES ('member@example.com', 'old');
             CREATE VIEW user_keys AS
             WITH original AS (
               SELECT email, CAST(email_identity_key(email) AS TEXT) AS canonical FROM users
             ), keys AS (SELECT email FROM original UNION SELECT canonical FROM original)
             SELECT email FROM keys;
             CREATE VIEW eligible_users AS SELECT email FROM users
             WHERE NOT EXISTS (
               SELECT 1 FROM user_keys WHERE user_keys.email = email_identity_key(users.email)
             );",
        )
        .unwrap();
        fs::write(
            migrations.join("001_remove_obsolete.sql"),
            "ALTER TABLE users DROP COLUMN obsolete;",
        )
        .unwrap();
        let init = temp.path().join("init.sql");
        fs::write(
            &init,
            "-- marmot: scalar email_identity_key 1\nINVALID INITIALIZATION SQL;",
        )
        .unwrap();

        let missing = migrate_connection(&conn, &migrations, TRACKING_TABLE).unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("no such function: email_identity_key")
        );
        assert!(applied_versions(&conn).is_empty());
        migrate_connection_with_init_sql(&conn, &migrations, TRACKING_TABLE, Some(&init)).unwrap();

        assert_eq!(applied_versions(&conn), ["001_remove_obsolete"]);
        assert_eq!(
            conn.query_row("SELECT email FROM users", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "member@example.com"
        );
    }

    #[test]
    fn declared_scalar_evaluation_rolls_back_the_data_migration() {
        let temp = tempfile::tempdir().unwrap();
        let migrations = temp.path().join("migrations");
        fs::create_dir(&migrations).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users (email TEXT);
             INSERT INTO users VALUES ('member@example.com');",
        )
        .unwrap();
        fs::write(
            migrations.join("001_update_email.sql"),
            "UPDATE users SET email = 'changed@example.com';
             UPDATE users SET email = email_identity_key(email);",
        )
        .unwrap();
        let init = temp.path().join("init.sql");
        fs::write(&init, "-- marmot: scalar email_identity_key 1").unwrap();

        let error =
            migrate_connection_with_init_sql(&conn, &migrations, TRACKING_TABLE, Some(&init))
                .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("analysis-only scalar cannot execute")
        );
        assert!(applied_versions(&conn).is_empty());
        assert_eq!(
            conn.query_row("SELECT email FROM users", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "member@example.com"
        );
        assert!(conn.is_autocommit());
    }

    #[test]
    fn deterministic_analysis_declaration_admits_empty_index_but_rejects_data() {
        for deterministic in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let migrations = temp.path().join("migrations");
            fs::create_dir(&migrations).unwrap();
            fs::write(
                migrations.join("001_spellings.sql"),
                "create table spellings(email text);
                 create index spelling_identity on spellings(identity_key(email));",
            )
            .unwrap();
            let init = temp.path().join("init.sql");
            fs::write(
                &init,
                format!(
                    "-- marmot: scalar identity_key 1{}\n",
                    if deterministic { " deterministic" } else { "" }
                ),
            )
            .unwrap();
            let conn = Connection::open_in_memory().unwrap();
            let result =
                migrate_connection_with_init_sql(&conn, &migrations, TRACKING_TABLE, Some(&init));
            if !deterministic {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("non-deterministic functions")
                );
                assert!(applied_versions(&conn).is_empty());
                assert!(conn.is_autocommit());
                continue;
            }
            result.unwrap();
            assert_eq!(applied_versions(&conn), ["001_spellings"]);
            conn.prepare("select email from spellings where identity_key(email)=?1")
                .unwrap();
            fs::write(
                migrations.join("002_seed.sql"),
                "insert into spellings values('member@example.test');",
            )
            .unwrap();
            let error =
                migrate_connection_with_init_sql(&conn, &migrations, TRACKING_TABLE, Some(&init))
                    .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("analysis-only scalar cannot execute")
            );
            assert_eq!(applied_versions(&conn), ["001_spellings"]);
            assert_eq!(
                conn.query_row("select count(*) from spellings", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            assert!(conn.is_autocommit());
        }
    }

    #[test]
    fn invalid_scalar_declarations_fail_before_migrations() {
        let temp = tempfile::tempdir().unwrap();
        let migrations = temp.path().join("migrations");
        fs::create_dir(&migrations).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        fs::write(
            migrations.join("001_users.sql"),
            "CREATE TABLE users (email TEXT);",
        )
        .unwrap();
        let init = temp.path().join("init.sql");
        fs::write(&init, "-- marmot: scalar email_identity_key invalid").unwrap();

        assert!(matches!(
            migrate_connection_with_init_sql(&conn, &migrations, TRACKING_TABLE, Some(&init)),
            Err(MigrationError::InitSqlDeclarationsError { .. })
        ));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn connection_migrations_preserve_real_application_functions() {
        let temp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.create_scalar_function(
            "email_identity_key",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            |context| Ok(context.get::<String>(0)?.to_lowercase()),
        )
        .unwrap();
        fs::write(
            temp.path().join("001_users.sql"),
            "CREATE TABLE users (email TEXT);
             INSERT INTO users VALUES (email_identity_key('MEMBER@EXAMPLE.COM'));",
        )
        .unwrap();

        migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();

        assert_eq!(
            conn.query_row("SELECT email FROM users", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "member@example.com"
        );
    }

    #[test]
    fn rebuild_migration_preserves_referencing_rows_and_restores_foreign_keys() {
        let temp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        fs::write(
            temp.path().join("001_create_orgs.sql"),
            "CREATE TABLE orgs (id INTEGER PRIMARY KEY, obsolete TEXT);
             CREATE TABLE members (id INTEGER PRIMARY KEY, org_id INTEGER REFERENCES orgs(id) ON DELETE CASCADE);",
        ).unwrap();
        migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();
        conn.execute_batch(
            "INSERT INTO orgs VALUES (7, 'old'); INSERT INTO members VALUES (9, 7);",
        )
        .unwrap();
        fs::write(
            temp.path().join("002_rebuild_orgs.sql"),
            "\u{feff}-- marmot: rebuild-table
CREATE TABLE new_orgs (id INTEGER PRIMARY KEY);
             INSERT INTO new_orgs SELECT id FROM orgs; DROP TABLE orgs; ALTER TABLE new_orgs RENAME TO orgs;",
        ).unwrap();

        migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();

        assert_eq!(
            conn.query_row("SELECT org_id FROM members WHERE id = 9", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(
            conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn rebuild_migration_rolls_back_invalid_rows_and_sql_and_restores_foreign_keys() {
        for replacement in [
            "INSERT INTO new_orgs VALUES (8); DROP TABLE orgs; ALTER TABLE new_orgs RENAME TO orgs;",
            "INSERT INTO new_orgs SELECT id FROM orgs; DROP TABLE orgs; SELECT missing_column FROM missing_table;",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let conn = Connection::open_in_memory().unwrap();
            fs::write(temp.path().join("001_create_orgs.sql"),
                "CREATE TABLE orgs (id INTEGER PRIMARY KEY, obsolete TEXT); CREATE TABLE members (id INTEGER PRIMARY KEY, org_id INTEGER REFERENCES orgs(id) ON DELETE CASCADE);").unwrap();
            migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();
            conn.execute_batch(
                "INSERT INTO orgs VALUES (7, 'old'); INSERT INTO members VALUES (9, 7);",
            )
            .unwrap();
            fs::write(temp.path().join("002_rebuild_orgs.sql"), format!("-- marmot: rebuild-table\nCREATE TABLE new_orgs (id INTEGER PRIMARY KEY); {replacement}")).unwrap();
            assert!(migrate_connection(&conn, temp.path(), TRACKING_TABLE).is_err());
            assert_eq!(applied_versions(&conn), ["001_create_orgs"]);
            assert_eq!(
                conn.query_row("SELECT obsolete FROM orgs WHERE id=7", [], |row| row
                    .get::<_, String>(0))
                    .unwrap(),
                "old"
            );
            assert_eq!(
                conn.query_row("SELECT org_id FROM members WHERE id=9", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                7
            );
            assert_eq!(
                conn.pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert!(conn.is_autocommit());
        }
    }

    #[test]
    fn rebuild_migration_cannot_commit_changes_before_later_failure() {
        for escape in [
            "COMMIT;",
            "/* comment */ CoMmIt;",
            "\u{feff}COMMIT;",
            "END TRANSACTION;",
            "ROLLBACK; BEGIN;",
            "SAVEPOINT escape; RELEASE escape; COMMIT;",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let conn = Connection::open_in_memory().unwrap();
            fs::write(
                temp.path().join("001_create_orgs.sql"),
                "CREATE TABLE orgs (id INTEGER PRIMARY KEY, obsolete TEXT);",
            )
            .unwrap();
            migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();
            conn.execute("INSERT INTO orgs VALUES (7,'retained')", [])
                .unwrap();
            fs::write(temp.path().join("002_rebuild_orgs.sql"), format!("-- marmot: rebuild-table\nUPDATE orgs SET obsolete='lost'; {escape} SELECT missing FROM missing_table;")).unwrap();
            assert!(migrate_connection(&conn, temp.path(), TRACKING_TABLE).is_err());
            assert_eq!(
                conn.query_row("SELECT obsolete FROM orgs WHERE id=7", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "retained",
                "escape: {escape}"
            );
            assert_eq!(applied_versions(&conn), ["001_create_orgs"]);
            assert!(conn.is_autocommit());
            assert_eq!(
                conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn rebuild_migration_preserves_compound_triggers_and_transaction_words_in_values() {
        let temp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        fs::write(
            temp.path().join("001_create_records.sql"),
            "CREATE TABLE records (id INTEGER PRIMARY KEY, note TEXT);",
        )
        .unwrap();
        migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();
        fs::write(temp.path().join("002_rebuild_records.sql"),
            "-- marmot: rebuild-table\n-- COMMIT; in a comment is not a statement.\nCREATE TRIGGER record_note AFTER INSERT ON records BEGIN UPDATE records SET note=CASE WHEN new.id=1 THEN 'COMMIT; END TRANSACTION;' ELSE 'ROLLBACK;' END WHERE id=new.id; END; INSERT INTO records VALUES (1, 'BEGIN;');"
        ).unwrap();
        migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();
        assert_eq!(
            conn.query_row("SELECT note FROM records WHERE id=1", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "COMMIT; END TRANSACTION;"
        );
    }

    #[test]
    fn rebuild_migration_bom_header_cannot_bypass_transaction_guard() {
        let temp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        fs::write(
            temp.path().join("001_create_orgs.sql"),
            "CREATE TABLE orgs (id INTEGER PRIMARY KEY, note TEXT);",
        )
        .unwrap();
        migrate_connection(&conn, temp.path(), TRACKING_TABLE).unwrap();
        conn.execute("INSERT INTO orgs VALUES (7,'retained')", [])
            .unwrap();
        fs::write(temp.path().join("002_rebuild_orgs.sql"), "\u{feff}-- marmot: rebuild-table\nUPDATE orgs SET note='lost'; COMMIT; SELECT missing FROM missing_table;").unwrap();
        assert!(migrate_connection(&conn, temp.path(), TRACKING_TABLE).is_err());
        assert_eq!(
            conn.query_row("SELECT note FROM orgs WHERE id=7", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "retained"
        );
        assert_eq!(applied_versions(&conn), ["001_create_orgs"]);
    }

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
    fn successful_migrations_optimize_planner_statistics() {
        let temp = tempfile::tempdir().unwrap();
        let migrations_dir = temp.path().join("db/migrations");
        let database = temp.path().join("app.db");
        fs::create_dir_all(&migrations_dir).unwrap();
        fs::write(
            migrations_dir.join("001_create_teams.sql"),
            "create table teams (id integer primary key, name text not null);\
             create index teams_name on teams(name);\
             with recursive numbers(value) as (\
               select 1 union all select value + 1 from numbers where value < 1000\
             )\
             insert into teams(name) select printf('team-%04d', value) from numbers",
        )
        .unwrap();

        migrate_from(&database, &migrations_dir).unwrap();

        let conn = Connection::open(&database).unwrap();
        let rows: i64 = conn
            .query_row(
                "select count(*) from sqlite_stat1 where tbl = 'teams'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(rows > 0);
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
    fn atomic_rebuild_fixture() -> (tempfile::TempDir, Connection) {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("migrations")).unwrap();
        let conn = Connection::open(directory.path().join("fixture.db")).unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;
            CREATE TABLE schema_versions(version TEXT PRIMARY KEY,applied_at TEXT NOT NULL) STRICT;
            INSERT INTO schema_versions VALUES('001_baseline','2026-01-01');
            CREATE TABLE parents(id INTEGER PRIMARY KEY,note TEXT,obsolete TEXT) STRICT;
            CREATE TABLE children(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES parents(id) ON DELETE CASCADE) STRICT;
            INSERT INTO parents VALUES(1,'retained','old');INSERT INTO children VALUES(1,1);").unwrap();
        fs::write(
            directory.path().join("migrations/001_baseline.sql"),
            "SELECT 1;",
        )
        .unwrap();
        fs::write(
            directory.path().join("migrations/002_before.sql"),
            "UPDATE parents SET note='updated';",
        )
        .unwrap();
        fs::write(directory.path().join("migrations/003_rebuild.sql"),"-- marmot: rebuild-table\nCREATE TABLE new_parents(id INTEGER PRIMARY KEY,note TEXT) STRICT;INSERT INTO new_parents SELECT id,note FROM parents;DROP TABLE parents;ALTER TABLE new_parents RENAME TO parents;").unwrap();
        fs::write(directory.path().join("migrations/004_after.sql"),"CREATE TABLE final_marker(id INTEGER PRIMARY KEY) STRICT;INSERT INTO final_marker VALUES(1);").unwrap();
        (directory, conn)
    }

    #[test]
    fn atomic_batch_rebuild_preserves_children_and_restores_enforcement() {
        let (directory, mut conn) = atomic_rebuild_fixture();
        let applied = migrate_connection_atomically::<MigrationError>(
            &mut conn,
            directory.path().join("migrations"),
            "schema_versions",
            false,
            |conn| {
                assert!(
                    !conn
                        .pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                        .unwrap()
                );
                assert_eq!(
                    conn.query_row("SELECT count(*) FROM children", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    1
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(applied, ["002_before", "003_rebuild", "004_after"]);
        assert_eq!(
            conn.query_row("SELECT note FROM parents", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "updated"
        );
        assert!(
            conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                .unwrap()
        );
        assert!(
            conn.execute("INSERT INTO children VALUES(2,999)", [])
                .is_err()
        );
        let repeated = migrate_connection_atomically::<MigrationError>(
            &mut conn,
            directory.path().join("migrations"),
            "schema_versions",
            false,
            |conn| {
                assert!(
                    conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                        .unwrap(),
                    "an already-applied rebuild must not disable enforcement"
                );
                Ok(())
            },
        )
        .unwrap();
        assert!(repeated.is_empty());
    }

    #[test]
    fn atomic_batch_rebuild_failure_rolls_back_every_version_and_restores_enforcement() {
        for mode in ["sql", "foreign_key", "validation", "transaction_escape"] {
            let (directory, mut conn) = atomic_rebuild_fixture();
            let sql = match mode {
                "sql" => "INSERT INTO missing_table VALUES(1);",
                "foreign_key" => "INSERT INTO children VALUES(2,999);",
                "transaction_escape" => "COMMIT;CREATE TABLE escaped(id INTEGER);",
                "validation" => "CREATE TABLE final_marker(id INTEGER PRIMARY KEY) STRICT;",
                _ => unreachable!(),
            };
            fs::write(directory.path().join("migrations/004_after.sql"), sql).unwrap();
            let before = fs::read(directory.path().join("fixture.db")).unwrap();
            let validation_ran = std::cell::Cell::new(false);
            let result = migrate_connection_atomically::<MigrationError>(
                &mut conn,
                directory.path().join("migrations"),
                "schema_versions",
                false,
                |_| {
                    validation_ran.set(true);
                    if mode == "validation" {
                        return Err(MigrationError::MigrationSqlError {
                            path: directory.path().into(),
                            source: rusqlite::Error::InvalidQuery,
                        });
                    }
                    Ok(())
                },
            );
            let error = result.unwrap_err();
            assert_eq!(
                validation_ran.get(),
                matches!(mode, "foreign_key" | "validation"),
                "{mode}: {error}"
            );
            if mode == "sql" {
                assert!(
                    error.to_string().contains("004_after.sql")
                        && error.to_string().contains("no such table"),
                    "{error}"
                );
            }
            if mode == "foreign_key" {
                assert!(
                    error
                        .to_string()
                        .contains("release batch leaves invalid foreign keys"),
                    "{error}"
                );
            }
            assert!(conn.is_autocommit(), "{mode}");
            assert!(
                conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                    .unwrap(),
                "{mode}"
            );
            assert_eq!(
                conn.query_row("SELECT note||':'||obsolete FROM parents", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "retained:old",
                "{mode}"
            );
            assert_eq!(
                conn.query_row("SELECT count(*) FROM children", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1,
                "{mode}"
            );
            assert_eq!(
                conn.query_row("SELECT count(*) FROM schema_versions", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1,
                "{mode}"
            );
            assert_eq!(
                fs::read(directory.path().join("fixture.db")).unwrap(),
                before,
                "{mode}"
            );
        }
    }

    #[test]
    fn atomic_batch_rebuild_dry_run_keeps_the_database_byte_identical() {
        let (directory, mut conn) = atomic_rebuild_fixture();
        let before = fs::read(directory.path().join("fixture.db")).unwrap();
        let applied = migrate_connection_atomically::<MigrationError>(
            &mut conn,
            directory.path().join("migrations"),
            "schema_versions",
            true,
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(applied.len(), 3);
        assert_eq!(
            fs::read(directory.path().join("fixture.db")).unwrap(),
            before
        );
        assert!(
            conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                .unwrap()
        );
    }

    #[test]
    fn atomic_batch_without_rebuild_matches_the_existing_transaction_bytes() {
        let (directory, conn) = atomic_rebuild_fixture();
        drop(conn);
        fs::write(
            directory.path().join("migrations/003_rebuild.sql"),
            "ALTER TABLE parents ADD COLUMN extra TEXT;",
        )
        .unwrap();
        let old = directory.path().join("old.db");
        fs::copy(directory.path().join("fixture.db"), &old).unwrap();
        let mut previous = Connection::open(old).unwrap();
        let mut current = Connection::open(directory.path().join("fixture.db")).unwrap();
        for conn in [&previous, &current] {
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            conn.create_scalar_function(
                "datetime",
                1,
                rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                |_| Ok("2026-01-01 00:00:00".to_owned()),
            )
            .unwrap();
        }
        let tx = previous
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let old_applied = migrate_connection_in_transaction(
            &tx,
            directory.path().join("migrations"),
            "schema_versions",
        )
        .unwrap();
        tx.commit().unwrap();
        let new_applied = migrate_connection_atomically::<MigrationError>(
            &mut current,
            directory.path().join("migrations"),
            "schema_versions",
            false,
            |conn| {
                assert!(
                    conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))
                        .unwrap()
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(old_applied, new_applied);
        drop(previous);
        drop(current);
        assert_eq!(
            fs::read(directory.path().join("old.db")).unwrap(),
            fs::read(directory.path().join("fixture.db")).unwrap()
        );
    }
}
