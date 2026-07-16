use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpResult {
    Unchanged,
    Written,
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("could not open SQLite database {path}: {source}")]
    DatabaseOpen {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("could not read SQLite schema from {path}: {source}")]
    SchemaRead {
        path: PathBuf,
        source: rusqlite::Error,
    },

    #[error("could not create schema dump directory {path}: {source}")]
    OutputDirectoryCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not read schema dump {path}: {source}")]
    OutputRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not write schema dump {path}: {source}")]
    OutputWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("schema dump is stale: {path}\nhint: run `marmot dump-schema --output {path}`")]
    Stale { path: PathBuf },
}

pub fn render(database_path: impl AsRef<Path>) -> Result<String, SchemaError> {
    let database_path = database_path.as_ref();
    let conn = Connection::open(database_path).map_err(|source| SchemaError::DatabaseOpen {
        path: database_path.to_path_buf(),
        source,
    })?;
    render_connection(&conn, database_path)
}

pub fn render_connection(
    conn: &Connection,
    database_path: impl AsRef<Path>,
) -> Result<String, SchemaError> {
    let database_path = database_path.as_ref();
    let mut statement = conn
        .prepare(
            "select schema.sql
             from sqlite_schema as schema
             where schema.sql is not null
               and schema.name not like 'sqlite_%'
               and schema.name not in (
                 select name from pragma_table_list where type = 'shadow'
               )
             order by
               case schema.type
                 when 'table' then 1
                 when 'index' then 2
                 when 'view' then 3
                 when 'trigger' then 4
                 else 5
               end,
               schema.name",
        )
        .map_err(|source| SchemaError::SchemaRead {
            path: database_path.to_path_buf(),
            source,
        })?;
    let statements = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| SchemaError::SchemaRead {
            path: database_path.to_path_buf(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SchemaError::SchemaRead {
            path: database_path.to_path_buf(),
            source,
        })?;

    let mut output = String::from("-- Generated schema. Do not edit.\n\n");
    for (index, statement) in statements.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str(statement.trim_end().trim_end_matches(';'));
        output.push_str(";\n");
    }
    Ok(output)
}

pub fn dump(
    database_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    check: bool,
) -> Result<DumpResult, SchemaError> {
    let output_path = output_path.as_ref();
    let rendered = render(database_path)?;
    match fs::read_to_string(output_path) {
        Ok(existing) if existing == rendered => return Ok(DumpResult::Unchanged),
        Ok(_) if check => {
            return Err(SchemaError::Stale {
                path: output_path.to_path_buf(),
            });
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound && check => {
            return Err(SchemaError::Stale {
                path: output_path.to_path_buf(),
            });
        }
        Err(source) if source.kind() != std::io::ErrorKind::NotFound => {
            return Err(SchemaError::OutputRead {
                path: output_path.to_path_buf(),
                source,
            });
        }
        _ => {}
    }

    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| SchemaError::OutputDirectoryCreate {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(output_path, rendered).map_err(|source| SchemaError::OutputWrite {
        path: output_path.to_path_buf(),
        source,
    })?;
    Ok(DumpResult::Written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_schema_in_dependency_safe_order_without_internal_objects() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table parents (id integer primary key);
             create table children (
               id integer primary key autoincrement,
               parent_id integer not null references parents(id)
             );
             create index index_children_on_parent_id on children(parent_id);
             create view view_children (id) as select id from children;
             create trigger delete_children after delete on parents
             begin
               delete from children where parent_id = old.id;
             end;",
        )
        .unwrap();

        let dump = render_connection(&conn, "<memory>").unwrap();

        assert!(dump.starts_with("-- Generated schema. Do not edit.\n\n"));
        assert!(!dump.contains("sqlite_sequence"));
        assert!(dump.find("CREATE TABLE parents").unwrap() < dump.find("CREATE INDEX").unwrap());
        assert!(dump.find("CREATE INDEX").unwrap() < dump.find("CREATE VIEW").unwrap());
        assert!(dump.find("CREATE VIEW").unwrap() < dump.find("CREATE TRIGGER").unwrap());
        assert!(!dump.ends_with("\n\n"));

        let restored = Connection::open_in_memory().unwrap();
        restored.execute_batch(&dump).unwrap();
        assert_eq!(
            restored
                .query_row("select count(*) from view_children", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn check_rejects_a_stale_dump_without_rewriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("app.sqlite3");
        let output = temp.path().join("db/schema.sql");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table users (id integer primary key);")
            .unwrap();
        drop(conn);

        assert_eq!(
            dump(&database, &output, false).unwrap(),
            DumpResult::Written
        );
        assert_eq!(
            dump(&database, &output, true).unwrap(),
            DumpResult::Unchanged
        );
        fs::write(&output, "stale\n").unwrap();

        assert!(matches!(
            dump(&database, &output, true),
            Err(SchemaError::Stale { path }) if path == output
        ));
        assert_eq!(fs::read_to_string(&output).unwrap(), "stale\n");
    }
}
