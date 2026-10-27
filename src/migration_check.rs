//! Replay numbered migrations without an application database, then audit the schema.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use crate::config::{ConfigError, FileConfig};
use crate::migrations::{self, MigrationError};
use crate::sqlite::tokenize::{Token, tokenize};

#[derive(Debug, thiserror::Error)]
pub enum MigrationCheckError {
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("could not inspect migration schema: {0}")]
    Schema(#[from] rusqlite::Error),
    #[error(transparent)]
    BooleanMarker(#[from] crate::Error),
    #[error("migration schema violates SQL policy: {0}")]
    Policy(String),
}

/// The nearest ancestor marmot.toml owns scalar declarations and structural
/// trigger exceptions. Paths in it resolve relative to that file.
/// No configured database, bootstrap, seed or initialization SQL is executed.
pub fn check_from(directory: impl AsRef<Path>) -> Result<Vec<String>, MigrationCheckError> {
    let directory = directory.as_ref();
    // Discover first, including duplicate detection, before any replay or config.
    migrations::versions_from(directory)?;
    let config_path = directory
        .ancestors()
        .map(|parent| parent.join("marmot.toml"))
        .find(|path| path.is_file());
    let config = match &config_path {
        Some(path) => FileConfig::read_optional(path)?,
        None => FileConfig::default(),
    };
    let init_sql = config
        .init_sql
        .as_ref()
        .map(|path| config_path.as_ref().unwrap().parent().unwrap().join(path));
    let conn = Connection::open_in_memory()?;
    let versions = migrations::migrate_connection_with_init_sql(
        &conn,
        directory,
        config
            .migration_table
            .as_deref()
            .unwrap_or(migrations::TRACKING_TABLE),
        init_sql.as_deref(),
    )?;
    check_schema(&conn, &config)?;
    Ok(versions)
}

fn check_schema(conn: &Connection, config: &FileConfig) -> Result<(), MigrationCheckError> {
    let mut statement = conn.prepare(
        "SELECT type, name, sql FROM sqlite_schema
         WHERE type IN ('table', 'trigger') AND sql IS NOT NULL ORDER BY type, name",
    )?;
    let objects = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for object in objects {
        let (kind, name, sql) = object?;
        let tokens = tokenize(&sql);
        let word = |token: &Token, expected: &str| matches!(token, Token::Word(value) if value.eq_ignore_ascii_case(expected));
        if kind == "trigger" {
            if !config.structural_triggers.contains(&name)
                || tokens.iter().any(|token| word(token, "raise"))
            {
                return Err(MigrationCheckError::Policy(format!(
                    "trigger {name} is not an allowed structural trigger"
                )));
            }
            continue;
        }
        let mut columns = conn.prepare("SELECT name, type FROM pragma_table_xinfo(?1)")?;
        let declared_types = columns
            .query_map([&name], |row| {
                Ok((row.get::<_, String>(0)?.to_ascii_lowercase(), row.get(1)?))
            })?
            .collect::<rusqlite::Result<BTreeMap<String, String>>>()?;
        // Reuse Marmot's marker validation, including column type and IN (0, 1).
        crate::analyzer::named_boolean_columns(&name, &sql, &declared_types)?;
        for (index, token) in tokens.iter().enumerate() {
            if word(token, "check") && matches!(tokens.get(index + 1), Some(Token::OpenParen)) {
                let named_boolean = index >= 2
                    && word(&tokens[index - 2], "constraint")
                    && matches!(&tokens[index - 1], Token::Word(value) | Token::QuotedId(value)
                        if value.eq_ignore_ascii_case("boolean"));
                if !named_boolean {
                    return Err(MigrationCheckError::Policy(format!(
                        "table {name} has a CHECK outside the named Boolean marker"
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(sql: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("migrations")).unwrap();
        fs::write(dir.path().join("migrations/001_schema.sql"), sql).unwrap();
        dir
    }

    #[test]
    fn replay_reports_the_failing_file_for_syntax_and_missing_objects() {
        for sql in [
            "not SQL",
            "INSERT INTO missing_table VALUES (1)",
            "CREATE TABLE items(id INTEGER); SELECT missing_column FROM items",
        ] {
            let dir = fixture(sql);
            let error = check_from(dir.path().join("migrations"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("001_schema.sql"), "{error}");
        }
    }

    #[test]
    fn policy_rejects_checks_regardless_of_spacing_or_comments_and_spoofed_markers() {
        for sql in [
            "CREATE TABLE items(id INTEGER CHECK(id > 0))",
            "CREATE TABLE items(id INTEGER CHECK\n/* comment */(id > 0))",
            "CREATE TABLE items(id INTEGER CONSTRAINT boolean CHECK(id > 0))",
            "CREATE TABLE items(id INTEGER, CONSTRAINT boolean CHECK(id IN (0,1)))",
        ] {
            let dir = fixture(sql);
            let error = check_from(dir.path().join("migrations"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("items"), "{error}");
        }
    }

    #[test]
    fn policy_allows_only_boolean_markers_and_ignores_check_text_in_literals() {
        let dir = fixture(
            "CREATE TABLE items(\n            enabled INTEGER CONSTRAINT \"boolean\" CHECK(enabled IN (0,1)),\n            note TEXT DEFAULT 'CHECK (value > 0)'\n        ) STRICT",
        );
        assert_eq!(
            check_from(dir.path().join("migrations")).unwrap(),
            ["001_schema"]
        );
    }

    #[test]
    fn policy_checks_final_schema_after_historical_constraints_are_removed() {
        let dir = fixture("CREATE TABLE items(id INTEGER CHECK(id > 0));");
        fs::write(
            dir.path().join("migrations/002_replace.sql"),
            "DROP TABLE items; CREATE TABLE items(id INTEGER);",
        )
        .unwrap();
        assert!(check_from(dir.path().join("migrations")).is_ok());
    }

    #[test]
    fn structural_trigger_exceptions_are_explicit_and_cannot_raise_validation_errors() {
        for (name, body, allowed) in [
            ("maintain_index", "INSERT INTO search VALUES(new.id);", true),
            ("business_rule", "INSERT INTO search VALUES(new.id);", false),
            ("maintain_index", "SELECT RAISE(ABORT, 'invalid');", false),
        ] {
            let dir = fixture(&format!(
                "CREATE TABLE items(id INTEGER); CREATE TABLE search(id INTEGER);\n                CREATE TRIGGER {name} AFTER INSERT ON items BEGIN {body} END;"
            ));
            fs::write(
                dir.path().join("marmot.toml"),
                "[tools.marmot]\nstructural_triggers = ['maintain_index']\n",
            )
            .unwrap();
            let result = check_from(dir.path().join("migrations"));
            assert_eq!(result.is_ok(), allowed, "{result:?}");
            if let Err(error) = result {
                assert!(error.to_string().contains(name));
            }
        }
    }

    #[test]
    fn replay_registers_declared_scalars_without_executing_init_sql_or_opening_database() {
        let dir = fixture(
            "CREATE TABLE items(id INTEGER, email TEXT, obsolete TEXT);\n            CREATE VIEW keys AS SELECT email_identity_key(email) FROM items;",
        );
        fs::write(
            dir.path().join("migrations/002_remove.sql"),
            "ALTER TABLE items DROP COLUMN obsolete;",
        )
        .unwrap();
        fs::write(
            dir.path().join("marmot.toml"),
            "[tools.marmot]\ninit_sql = 'init.sql'\ndatabase = 'never-open.sqlite'\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("init.sql"),
            "-- marmot: scalar email_identity_key 1 deterministic\nNOT EXECUTABLE SQL;",
        )
        .unwrap();
        assert!(check_from(dir.path().join("migrations")).is_ok());
        assert!(!dir.path().join("never-open.sqlite").exists());
    }
}
