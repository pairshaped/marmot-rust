use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use heck::ToSnakeCase;
use rusqlite::Connection;

use crate::config::Config;
use crate::discovery::discover_sql_files;
use crate::error::{Error, Result};
use crate::model::{Column, Parameter, Project, Query, ReturnType, ValueType, sanitize_identifier};
use crate::sql_text::validate_sql;
use crate::sqlite::tokenize::{Token, tokenize};

pub fn analyze_project(config: &Config) -> Result<Project> {
    let conn = Connection::open(&config.database).map_err(|source| Error::OpenDatabase {
        path: config.database.clone(),
        source,
    })?;
    let schema = load_schema(&conn)?;
    let files = discover_sql_files(&config.source_root)?;
    let mut queries = Vec::with_capacity(files.len());

    for file in files {
        let sql = fs::read_to_string(&file.path).map_err(|source| Error::ReadFile {
            path: file.path.clone(),
            source,
        })?;
        let sql = validate_sql(&sql).map_err(|reason| Error::InvalidSql {
            path: file.path.clone(),
            reason,
        })?;
        let parameters = named_parameters(&sql);
        let columns = result_columns(&conn, &schema, &file.path, &sql)?;
        let return_type = if columns.is_empty() {
            ReturnType::Execute
        } else {
            ReturnType::Rows { row_type: None }
        };

        queries.push(Query {
            source_path: file.path,
            module_name: file.module_name,
            name: file.query_name,
            return_type,
            sql,
            parameters,
            columns,
        });
    }

    Ok(Project { queries })
}

#[derive(Debug, Default)]
struct Schema {
    tables: BTreeMap<String, BTreeMap<String, SchemaColumn>>,
}

impl Schema {
    fn column(&self, table: &str, column: &str) -> Option<&SchemaColumn> {
        self.tables
            .get(&table.to_ascii_lowercase())
            .and_then(|columns| columns.get(&column.to_ascii_lowercase()))
    }
}

#[derive(Debug)]
struct SchemaColumn {
    declared_type: String,
    nullable: bool,
}

fn load_schema(conn: &Connection) -> Result<Schema> {
    let table_names = {
        let mut stmt = conn
            .prepare(
                "
                select name
                from sqlite_schema
                where type in ('table', 'view')
                  and name not like 'sqlite_%'
                order by name
                ",
            )
            .map_err(|source| Error::InspectDatabase { source })?;
        stmt.query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| Error::InspectDatabase { source })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|source| Error::InspectDatabase { source })?
    };

    let mut schema = Schema::default();

    for table_name in table_names {
        let mut stmt = conn
            .prepare(
                r#"
                select name, type, "notnull", pk
                from pragma_table_xinfo(?1)
                where hidden = 0
                "#,
            )
            .map_err(|source| Error::InspectDatabase { source })?;
        let columns = stmt
            .query_map([table_name.as_str()], |row| {
                let name: String = row.get(0)?;
                let declared_type: String = row.get(1)?;
                let notnull: i64 = row.get(2)?;
                let primary_key: i64 = row.get(3)?;
                Ok((
                    name.to_ascii_lowercase(),
                    SchemaColumn {
                        declared_type,
                        nullable: notnull == 0 && primary_key == 0,
                    },
                ))
            })
            .map_err(|source| Error::InspectDatabase { source })?
            .collect::<std::result::Result<BTreeMap<_, _>, _>>()
            .map_err(|source| Error::InspectDatabase { source })?;

        schema
            .tables
            .insert(table_name.to_ascii_lowercase(), columns);
    }

    Ok(schema)
}

fn named_parameters(sql: &str) -> Vec<Parameter> {
    let mut params: Vec<Parameter> = Vec::new();
    for token in tokenize(sql) {
        if let Token::ParamNamed { prefix, name } = token {
            add_parameter(&mut params, &name, &format!("{prefix}{name}"));
        }
    }

    params
}

fn add_parameter(params: &mut Vec<Parameter>, raw_name: &str, sql_name: &str) {
    let name = raw_name.to_snake_case();
    let sql_name = sql_name.to_string();
    if let Some(param) = params.iter_mut().find(|param| param.name == name) {
        if !param.sql_names.contains(&sql_name) {
            param.sql_names.push(sql_name);
        }
    } else {
        params.push(Parameter {
            name,
            sql_names: vec![sql_name],
        });
    }
}

fn result_columns(
    conn: &Connection,
    schema: &Schema,
    path: &std::path::Path,
    sql: &str,
) -> Result<Vec<Column>> {
    let stmt = conn.prepare(sql).map_err(|source| Error::PrepareSql {
        path: path.to_path_buf(),
        source,
    })?;
    let mut seen = BTreeSet::new();
    let mut duplicate_names = BTreeSet::new();
    let mut seen_field_names = BTreeSet::new();
    let mut duplicate_field_names = BTreeSet::new();
    let mut columns = Vec::new();
    let metadata = stmt.columns_with_metadata();

    for index in 0..stmt.column_count() {
        let name = metadata
            .get(index)
            .map(|column| column.name().to_string())
            .unwrap_or_else(|| format!("column_{index}"));
        if !seen.insert(name.clone()) {
            duplicate_names.insert(name.clone());
        }

        let mut field_name = sanitize_identifier(&name);
        if field_name.is_empty() {
            field_name = format!("column_{index}");
        }
        if !seen_field_names.insert(field_name.clone()) {
            duplicate_field_names.insert(field_name.clone());
        }
        let schema_column = metadata
            .get(index)
            .and_then(|column| Some((column.table_name()?, column.origin_name()?)))
            .and_then(|(table, column)| schema.column(table, column));
        let column_type = schema_column
            .map(|column| ValueType::from_sqlite_type(&column.declared_type))
            .unwrap_or_else(|| infer_column_type(&name));
        let nullable = schema_column
            .map(|column| column.nullable)
            .unwrap_or_else(|| infer_expression_nullability(&name));
        columns.push(Column {
            name,
            field_name,
            column_type,
            nullable,
        });
    }

    if !duplicate_names.is_empty() {
        return Err(Error::DuplicateColumns {
            path: path.to_path_buf(),
            columns: duplicate_names.into_iter().collect(),
        });
    }

    if !duplicate_field_names.is_empty() {
        return Err(Error::GeneratedColumnNameCollision {
            path: path.to_path_buf(),
            columns: duplicate_field_names.into_iter().collect(),
        });
    }

    Ok(columns)
}

fn infer_column_type(name: &str) -> ValueType {
    let normalized = name.trim().to_ascii_lowercase();
    if normalized == "id"
        || normalized.ends_with("_id")
        || normalized == "counter"
        || normalized.contains("count(")
        || normalized.contains("sum(")
        || normalized.contains("coalesce(")
    {
        return ValueType::I64;
    }
    ValueType::Value
}

fn infer_expression_nullability(name: &str) -> bool {
    let normalized = name.trim().to_ascii_lowercase();
    !(normalized.contains("count(") || normalized.contains("coalesce("))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Target;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn extracts_unique_named_parameters_in_encounter_order() {
        let params = named_parameters("where org_id = @org_id or parent_id = @org_id and x = @x");
        let names = params
            .into_iter()
            .map(|param| (param.name, param.sql_names))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("org_id".to_string(), vec!["@org_id".to_string()]),
                ("x".to_string(), vec!["@x".to_string()])
            ]
        );
    }

    #[test]
    fn extracts_mixed_named_parameter_prefixes_as_one_argument_per_name() {
        let params = named_parameters(
            "where user_id = @user_id and created_at >= :since and name like $pattern or id = :user_id",
        );
        let names = params
            .into_iter()
            .map(|param| (param.name, param.sql_names))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                (
                    "user_id".to_string(),
                    vec!["@user_id".to_string(), ":user_id".to_string()]
                ),
                ("since".to_string(), vec![":since".to_string()]),
                ("pattern".to_string(), vec!["$pattern".to_string()])
            ]
        );
    }

    #[test]
    fn ignores_named_parameter_tokens_inside_strings_identifiers_and_comments() {
        let params = named_parameters(
            r#"
            select '@not_param', ":also_not_param", id
            from users
            where name = @name -- @comment_param
              and bio = 'literal :still_not_param'
              and note = /* $block_param */ $note
            "#,
        );
        let names = params
            .into_iter()
            .map(|param| param.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["name", "note"]);
    }

    #[test]
    fn infers_common_integer_columns() {
        assert_eq!(infer_column_type("id"), ValueType::I64);
        assert_eq!(infer_column_type("count(*)"), ValueType::I64);
        assert_eq!(infer_column_type("coalesce(sum(id), 0)"), ValueType::I64);
        assert_eq!(infer_column_type("name"), ValueType::Value);
    }

    #[test]
    fn analyzes_result_column_types_from_sqlite_schema() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table items (
                id integer primary key,
                org_id integer not null,
                name text not null,
                description text,
                active boolean not null,
                price real,
                payload blob
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_items.sql"),
            "
            select id, org_id, name, description, active, price, payload
            from items
            where org_id = @org_id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let columns = &project.queries[0].columns;
        let facts = columns
            .iter()
            .map(|column| {
                (
                    column.field_name.as_str(),
                    column.column_type.clone(),
                    column.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("id", ValueType::I64, false),
                ("org_id", ValueType::I64, false),
                ("name", ValueType::String, false),
                ("description", ValueType::String, true),
                ("active", ValueType::Bool, false),
                ("price", ValueType::F64, true),
                ("payload", ValueType::Bytes, true),
            ]
        );
    }

    #[test]
    fn rejects_sql_files_with_multiple_statements() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        Connection::open(&database).unwrap();

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(sql_dir.join("bad.sql"), "select 1; select 2").unwrap();

        let result = analyze_project(&Config {
            database,
            source_root,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        });

        assert!(matches!(result, Err(Error::InvalidSql { .. })));
    }

    #[test]
    fn rejects_duplicate_result_column_names() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table users (id integer primary key, name text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(sql_dir.join("bad.sql"), "select id, id from users").unwrap();

        let result = analyze_project(&Config {
            database,
            source_root,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        });

        assert!(matches!(
            result,
            Err(Error::DuplicateColumns { columns, .. }) if columns == ["id"]
        ));
    }

    #[test]
    fn rejects_generated_result_column_name_collisions() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        Connection::open(&database).unwrap();

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("bad.sql"),
            r#"select 1 as "foo-bar", 2 as foo_bar"#,
        )
        .unwrap();

        let result = analyze_project(&Config {
            database,
            source_root,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        });

        assert!(matches!(
            result,
            Err(Error::GeneratedColumnNameCollision { columns, .. }) if columns == ["foo_bar"]
        ));
    }
}
