use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use heck::ToSnakeCase;
use rusqlite::Connection;

use crate::config::Config;
use crate::discovery::discover_sql_files;
use crate::error::{Error, Result};
use crate::model::{Column, Parameter, Project, Query, ReturnType, ValueType, sanitize_identifier};
use crate::sql_text::validate_sql;
use crate::sqlite::annotation::parse_returns_annotation;
use crate::sqlite::tokenize::{SpannedToken, Token, tokenize, tokenize_spans};

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
        let row_type =
            parse_returns_annotation(&sql).map_err(|reason| Error::InvalidReturnsAnnotation {
                path: file.path.clone(),
                reason,
            })?;
        let sqlite_sql = strip_nullability_overrides(&sql);
        let parameters = named_parameters(&sqlite_sql);
        let columns = result_columns(&conn, &schema, &file.path, &sql, &sqlite_sql)?;
        let return_type = if columns.is_empty() {
            ReturnType::Execute
        } else {
            ReturnType::Rows { row_type }
        };

        queries.push(Query {
            source_path: file.path,
            module_name: file.module_name,
            name: file.query_name,
            return_type,
            sql: sqlite_sql,
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
    sqlite_sql: &str,
) -> Result<Vec<Column>> {
    let stmt = conn
        .prepare(sqlite_sql)
        .map_err(|source| Error::PrepareSql {
            path: path.to_path_buf(),
            source,
        })?;
    let mut seen = BTreeSet::new();
    let mut duplicate_names = BTreeSet::new();
    let mut seen_field_names = BTreeSet::new();
    let mut duplicate_field_names = BTreeSet::new();
    let mut columns = Vec::new();
    let metadata = stmt.columns_with_metadata();
    let nullable_tables = left_join_nullable_tables(sql);
    let expression_inferences = select_expression_inferences(sql);

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
        let expression_inference = expression_inferences.get(&name.to_ascii_lowercase());
        let column_type = schema_column
            .map(|column| ValueType::from_sqlite_type(&column.declared_type))
            .or_else(|| expression_inference.and_then(|inference| inference.column_type.clone()))
            .unwrap_or_else(|| infer_column_type(&name));
        let table_nullable = metadata
            .get(index)
            .and_then(|column| column.table_name())
            .filter(|table| nullable_tables.contains(&table.to_ascii_lowercase()))
            .map(|_| true);
        let nullable = expression_inference
            .and_then(|inference| inference.nullable_override)
            .or(table_nullable)
            .or_else(|| schema_column.map(|column| column.nullable))
            .or_else(|| expression_inference.and_then(|inference| inference.inferred_nullable))
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

fn left_join_nullable_tables(sql: &str) -> BTreeSet<String> {
    let tokens = tokenize(sql);
    let mut nullable_tables = BTreeSet::new();
    let mut index = 0;

    while index < tokens.len() {
        if !token_is_word(&tokens[index], "LEFT") {
            index += 1;
            continue;
        }

        let mut join_index = index + 1;
        if tokens
            .get(join_index)
            .is_some_and(|token| token_is_word(token, "OUTER"))
        {
            join_index += 1;
        }

        if !tokens
            .get(join_index)
            .is_some_and(|token| token_is_word(token, "JOIN"))
        {
            index += 1;
            continue;
        }

        if let Some(table_name) = tokens.get(join_index + 1).and_then(table_name_from_token) {
            nullable_tables.insert(table_name.to_ascii_lowercase());
        }

        index = join_index + 1;
    }

    nullable_tables
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpressionInference {
    column_type: Option<ValueType>,
    inferred_nullable: Option<bool>,
    nullable_override: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Alias {
    name: String,
    nullable_override: Option<bool>,
}

fn select_expression_inferences(sql: &str) -> BTreeMap<String, ExpressionInference> {
    let tokens = tokenize(sql);
    let Some(select_list) = top_level_select_list(&tokens) else {
        return BTreeMap::new();
    };
    let mut inferences = BTreeMap::new();

    for expression in split_top_level_commas(select_list) {
        let Some(alias) = expression_alias(expression) else {
            continue;
        };
        let expression = expression_without_alias(expression);
        let expression_inference = infer_expression_tokens(expression);
        if expression_inference.is_some() || alias.nullable_override.is_some() {
            let mut inference = expression_inference.unwrap_or(ExpressionInference {
                column_type: None,
                inferred_nullable: None,
                nullable_override: None,
            });
            inference.nullable_override = alias.nullable_override;
            inferences.insert(alias.name.to_ascii_lowercase(), inference);
        }
    }

    inferences
}

fn top_level_select_list(tokens: &[Token]) -> Option<&[Token]> {
    let mut depth = 0usize;
    let mut select_start = None;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case("SELECT") => {
                select_start = Some(index + 1);
            }
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case("FROM") => {
                if let Some(start) = select_start {
                    return Some(&tokens[start..index]);
                }
            }
            _ => {}
        }
    }

    select_start.map(|start| &tokens[start..])
}

fn split_top_level_commas(tokens: &[Token]) -> Vec<&[Token]> {
    let mut expressions = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                expressions.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }

    if start < tokens.len() {
        expressions.push(&tokens[start..]);
    }

    expressions
}

fn expression_alias(tokens: &[Token]) -> Option<Alias> {
    let mut depth = 0usize;
    let mut alias = None;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case("AS") => {
                alias = tokens
                    .get(index + 1)
                    .and_then(identifier_from_token)
                    .map(|name| {
                        let nullable_override = match tokens.get(index + 2) {
                            Some(Token::NullOverride) => Some(false),
                            Some(Token::NullableOverride) => Some(true),
                            _ => None,
                        };
                        Alias {
                            name: name.to_string(),
                            nullable_override,
                        }
                    });
            }
            _ => {}
        }
    }

    alias
}

fn expression_without_alias(tokens: &[Token]) -> &[Token] {
    let mut depth = 0usize;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case("AS") => {
                return &tokens[..index];
            }
            _ => {}
        }
    }

    tokens
}

fn infer_expression_tokens(tokens: &[Token]) -> Option<ExpressionInference> {
    if tokens
        .first()
        .is_some_and(|token| token_is_word(token, "CASE"))
    {
        return infer_case_expression(tokens);
    }

    let function_name = match (tokens.first(), tokens.get(1)) {
        (Some(Token::Word(name)), Some(Token::OpenParen)) => name,
        _ => return None,
    };
    let function_name = function_name.to_ascii_lowercase();

    match function_name.as_str() {
        "row_number" | "rank" | "dense_rank" | "ntile" => Some(ExpressionInference {
            column_type: Some(ValueType::I64),
            inferred_nullable: Some(false),
            nullable_override: None,
        }),
        "count" => Some(ExpressionInference {
            column_type: Some(ValueType::I64),
            inferred_nullable: Some(false),
            nullable_override: None,
        }),
        "sum" | "avg" => Some(ExpressionInference {
            column_type: Some(ValueType::F64),
            inferred_nullable: Some(true),
            nullable_override: None,
        }),
        _ => None,
    }
}

fn infer_case_expression(tokens: &[Token]) -> Option<ExpressionInference> {
    let mut branch_types = Vec::new();
    let mut nullable = false;
    let mut has_else = false;
    let mut index = 1usize;

    while index < tokens.len() {
        if token_is_word(&tokens[index], "THEN") {
            let start = index + 1;
            let end = case_branch_end(tokens, start);
            let branch = infer_case_branch(&tokens[start..end]);
            nullable |= branch.nullable;
            if let Some(column_type) = branch.column_type {
                branch_types.push(column_type);
            }
            index = end;
        } else if token_is_word(&tokens[index], "ELSE") {
            has_else = true;
            let start = index + 1;
            let end = case_branch_end(tokens, start);
            let branch = infer_case_branch(&tokens[start..end]);
            nullable |= branch.nullable;
            if let Some(column_type) = branch.column_type {
                branch_types.push(column_type);
            }
            index = end;
        } else {
            index += 1;
        }
    }

    if !has_else {
        nullable = true;
    }

    let case_type = common_case_branch_type(branch_types)?;
    nullable |= case_type.mixed;
    Some(ExpressionInference {
        column_type: Some(case_type.column_type),
        inferred_nullable: Some(nullable),
        nullable_override: None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaseBranch {
    column_type: Option<ValueType>,
    nullable: bool,
}

fn case_branch_end(tokens: &[Token], start: usize) -> usize {
    let mut paren_depth = 0usize;
    let mut nested_case_depth = 0usize;

    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::OpenParen => paren_depth += 1,
            Token::CloseParen => paren_depth = paren_depth.saturating_sub(1),
            Token::Word(word) if paren_depth == 0 && word.eq_ignore_ascii_case("CASE") => {
                nested_case_depth += 1;
            }
            Token::Word(word) if paren_depth == 0 && word.eq_ignore_ascii_case("END") => {
                if nested_case_depth == 0 {
                    return index;
                }
                nested_case_depth -= 1;
            }
            Token::Word(word)
                if paren_depth == 0
                    && nested_case_depth == 0
                    && (word.eq_ignore_ascii_case("WHEN") || word.eq_ignore_ascii_case("ELSE")) =>
            {
                return index;
            }
            _ => {}
        }
    }

    tokens.len()
}

fn infer_case_branch(tokens: &[Token]) -> CaseBranch {
    match tokens.first() {
        Some(Token::Number(number)) => CaseBranch {
            column_type: Some(number_value_type(number)),
            nullable: false,
        },
        Some(Token::StringLit(_)) => CaseBranch {
            column_type: Some(ValueType::String),
            nullable: false,
        },
        Some(Token::Word(word)) if word.eq_ignore_ascii_case("NULL") => CaseBranch {
            column_type: None,
            nullable: true,
        },
        Some(Token::Word(word)) if word.eq_ignore_ascii_case("CASE") => {
            match infer_case_expression(tokens) {
                Some(inference) => CaseBranch {
                    column_type: inference.column_type,
                    nullable: inference.inferred_nullable.unwrap_or(true),
                },
                None => CaseBranch {
                    column_type: None,
                    nullable: true,
                },
            }
        }
        _ => CaseBranch {
            column_type: None,
            nullable: true,
        },
    }
}

fn number_value_type(number: &str) -> ValueType {
    if number.contains('.') || number.contains('e') {
        ValueType::F64
    } else {
        ValueType::I64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaseType {
    column_type: ValueType,
    mixed: bool,
}

fn common_case_branch_type(branch_types: Vec<ValueType>) -> Option<CaseType> {
    let first = branch_types.first()?.clone();
    if branch_types.iter().all(|column_type| column_type == &first) {
        Some(CaseType {
            column_type: first,
            mixed: false,
        })
    } else {
        Some(CaseType {
            column_type: ValueType::String,
            mixed: true,
        })
    }
}

fn strip_nullability_overrides(sql: &str) -> String {
    let tokens = tokenize_spans(sql);
    if !tokens.iter().any(is_nullability_override) {
        return sql.to_string();
    }

    let mut stripped = String::with_capacity(sql.len());
    let mut copied_until = 0usize;
    for token in tokens.iter().filter(|token| is_nullability_override(token)) {
        stripped.push_str(&sql[copied_until..token.start]);
        copied_until = token.end;
    }
    stripped.push_str(&sql[copied_until..]);
    stripped
}

fn is_nullability_override(token: &SpannedToken) -> bool {
    matches!(token.token, Token::NullOverride | Token::NullableOverride)
}

fn token_is_word(token: &Token, expected: &str) -> bool {
    matches!(token, Token::Word(text) if text.eq_ignore_ascii_case(expected))
}

fn identifier_from_token(token: &Token) -> Option<&str> {
    match token {
        Token::Word(name) | Token::QuotedId(name) => Some(name.as_str()),
        _ => None,
    }
}

fn table_name_from_token(token: &Token) -> Option<&str> {
    identifier_from_token(token)
}

fn infer_column_type(name: &str) -> ValueType {
    let normalized = name.trim().to_ascii_lowercase();
    if normalized == "id"
        || normalized.ends_with("_id")
        || normalized == "counter"
        || normalized.contains("count(")
        || normalized.contains("coalesce(")
    {
        return ValueType::I64;
    }
    if normalized.contains("sum(") || normalized.contains("avg(") {
        return ValueType::F64;
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
        assert_eq!(infer_column_type("sum(id)"), ValueType::F64);
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

    #[test]
    fn uses_returns_annotation_as_row_type_name() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table orgs (id integer primary key, name text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orgs/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_org.sql"),
            "-- returns: OrgRow\nselect id, name from orgs",
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

        assert_eq!(
            project.queries[0].return_type,
            ReturnType::Rows {
                row_type: Some("OrgRow".to_string())
            }
        );
    }

    #[test]
    fn rejects_invalid_returns_annotation() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table orgs (id integer primary key);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orgs/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_org.sql"),
            "-- returns: Org\nselect id from orgs",
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
            Err(Error::InvalidReturnsAnnotation { .. })
        ));
    }

    #[test]
    fn left_join_marks_right_side_result_columns_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            create table profiles (
                id integer primary key,
                user_id integer not null,
                bio text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_users.sql"),
            "
            select u.id, u.name, p.bio
            from users u
            left join profiles p on p.user_id = u.id
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

        let facts = project.queries[0]
            .columns
            .iter()
            .map(|column| (column.field_name.as_str(), column.nullable))
            .collect::<Vec<_>>();

        assert_eq!(facts, [("id", false), ("name", false), ("bio", true)]);
    }

    #[test]
    fn inner_join_keeps_result_columns_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            create table profiles (
                id integer primary key,
                user_id integer not null,
                bio text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_users.sql"),
            "
            select u.name, p.bio
            from users u
            join profiles p on p.user_id = u.id
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

        let facts = project.queries[0]
            .columns
            .iter()
            .map(|column| (column.field_name.as_str(), column.nullable))
            .collect::<Vec<_>>();

        assert_eq!(facts, [("name", false), ("bio", false)]);
    }

    #[test]
    fn chained_left_joins_mark_each_right_side_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table a (id integer primary key, a_val text not null);
            create table b (id integer primary key, a_id integer not null, b_val text not null);
            create table c (id integer primary key, b_id integer not null, c_val text not null);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "
            select a.a_val, b.b_val, c.c_val
            from a
            left join b on b.a_id = a.id
            left join c on c.b_id = b.id
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

        let facts = project.queries[0]
            .columns
            .iter()
            .map(|column| (column.field_name.as_str(), column.nullable))
            .collect::<Vec<_>>();

        assert_eq!(facts, [("a_val", false), ("b_val", true), ("c_val", true)]);
    }

    #[test]
    fn mixed_inner_and_left_joins_only_mark_left_join_side_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table a (id integer primary key, a_val text not null);
            create table b (id integer primary key, a_id integer not null, b_val text not null);
            create table c (id integer primary key, b_id integer not null, c_val text not null);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "
            select a.a_val, b.b_val, c.c_val
            from a
            join b on b.a_id = a.id
            left join c on c.b_id = b.id
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

        let facts = project.queries[0]
            .columns
            .iter()
            .map(|column| (column.field_name.as_str(), column.nullable))
            .collect::<Vec<_>>();

        assert_eq!(facts, [("a_val", false), ("b_val", false), ("c_val", true)]);
    }

    #[test]
    fn row_number_window_function_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table items (
                id integer primary key,
                created_at integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("ranked_items.sql"),
            "
            select id, row_number() over (order by created_at) as position
            from items
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

        let facts = project.queries[0]
            .columns
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
                ("position", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn rank_window_function_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table items (
                id integer primary key,
                score integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("ranked_items.sql"),
            "
            select id, rank() over (order by score desc) as rk
            from items
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

        let facts = project.queries[0]
            .columns
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
            [("id", ValueType::I64, false), ("rk", ValueType::I64, false)]
        );
    }

    #[test]
    fn sum_returns_f64_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table payments (
                amount real not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("payments/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("sum_payments.sql"),
            "
            select sum(amount) as total
            from payments
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

        let facts = project.queries[0]
            .columns
            .iter()
            .map(|column| {
                (
                    column.field_name.as_str(),
                    column.column_type.clone(),
                    column.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(facts, [("total", ValueType::F64, true)]);
    }

    #[test]
    fn alias_bang_forces_result_column_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table users (id integer primary key, name text);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_users.sql"),
            "select name as name! from users",
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

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "name".to_string(),
                field_name: "name".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
        assert_eq!(project.queries[0].sql, "select name as name from users");
    }

    #[test]
    fn alias_question_forces_result_column_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table users (id integer primary key, name text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_users.sql"),
            "select name as name? from users",
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

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "name".to_string(),
                field_name: "name".to_string(),
                column_type: ValueType::String,
                nullable: true,
            }]
        );
        assert_eq!(project.queries[0].sql, "select name as name from users");
    }

    #[test]
    fn case_with_int_literals_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, active boolean not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case when active then 1 else 0 end as registered from t",
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

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "registered".to_string(),
                field_name: "registered".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn case_with_string_literals_returns_string_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, active boolean not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case when active then 'yes' else 'no' end as label from t",
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

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "label".to_string(),
                field_name: "label".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn case_without_else_returns_nullable_branch_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, active boolean not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case when active then 1 end as maybe_val from t",
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

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "maybe_val".to_string(),
                field_name: "maybe_val".to_string(),
                column_type: ValueType::I64,
                nullable: true,
            }]
        );
    }

    #[test]
    fn case_with_null_branch_returns_nullable_branch_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, active boolean not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case when active then 1 else null end as maybe_val from t",
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

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "maybe_val".to_string(),
                field_name: "maybe_val".to_string(),
                column_type: ValueType::I64,
                nullable: true,
            }]
        );
    }

    #[test]
    fn case_with_mixed_branch_types_falls_back_to_nullable_string() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, active boolean not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case when active then 1 else 'a' end as mixed from t",
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

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "mixed".to_string(),
                field_name: "mixed".to_string(),
                column_type: ValueType::String,
                nullable: true,
            }]
        );
    }
}
