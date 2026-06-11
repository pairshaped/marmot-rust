use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use heck::ToSnakeCase;
use rusqlite::Connection;

use crate::config::Config;
use crate::discovery::discover_sql_files_with_sql_dir;
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
    let files = discover_sql_files_with_sql_dir(&config.source_root, config.sql_dir.as_deref())?;
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
        validate_insert_values_counts(&sqlite_sql, &schema, &file.path)?;
        let parameters = parameters(&sqlite_sql, &schema);
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

#[derive(Debug, Clone, Default)]
struct Schema {
    tables: BTreeMap<String, BTreeMap<String, SchemaColumn>>,
}

impl Schema {
    fn has_table(&self, table: &str) -> bool {
        self.tables.contains_key(&table.to_ascii_lowercase())
    }

    fn column(&self, table: &str, column: &str) -> Option<&SchemaColumn> {
        self.tables
            .get(&table.to_ascii_lowercase())
            .and_then(|columns| columns.get(&column.to_ascii_lowercase()))
    }

    fn column_names_in_table_order(&self, table: &str) -> Vec<String> {
        let Some(columns) = self.tables.get(&table.to_ascii_lowercase()) else {
            return vec![];
        };
        let mut columns = columns
            .iter()
            .map(|(name, column)| (name, column.ordinal))
            .collect::<Vec<_>>();
        columns.sort_by_key(|(_, ordinal)| *ordinal);
        columns
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect()
    }
}

#[derive(Debug, Clone)]
struct SchemaColumn {
    declared_type: String,
    nullable: bool,
    notnull: bool,
    primary_key: bool,
    rowid_alias: bool,
    ordinal: i64,
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
                select cid, name, type, "notnull", pk
                from pragma_table_xinfo(?1)
                where hidden = 0
                "#,
            )
            .map_err(|source| Error::InspectDatabase { source })?;
        let mut column_rows = stmt
            .query_map([table_name.as_str()], |row| {
                let ordinal: i64 = row.get(0)?;
                let name: String = row.get(1)?;
                let declared_type: String = row.get(2)?;
                let notnull: i64 = row.get(3)?;
                let primary_key: i64 = row.get(4)?;
                Ok((
                    name.to_ascii_lowercase(),
                    SchemaColumn {
                        declared_type,
                        nullable: false,
                        notnull: notnull != 0,
                        primary_key: primary_key != 0,
                        rowid_alias: false,
                        ordinal,
                    },
                ))
            })
            .map_err(|source| Error::InspectDatabase { source })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|source| Error::InspectDatabase { source })?;

        let without_rowid = table_without_rowid(conn, &table_name)?;
        let primary_key_count = column_rows
            .iter()
            .filter(|(_, column)| column.primary_key)
            .count();
        for (_, column) in &mut column_rows {
            column.rowid_alias = !without_rowid
                && primary_key_count == 1
                && column.primary_key
                && column.declared_type.eq_ignore_ascii_case("INTEGER");
            column.nullable =
                !column.notnull && !column.rowid_alias && !(without_rowid && column.primary_key);
        }
        let columns = column_rows.into_iter().collect::<BTreeMap<_, _>>();

        schema
            .tables
            .insert(table_name.to_ascii_lowercase(), columns);
    }

    Ok(schema)
}

fn schema_with_ctes(tokens: &[Token], schema: &Schema) -> Schema {
    let mut schema = schema.clone();
    add_ctes_to_schema(tokens, &mut schema);
    schema
}

fn add_ctes_to_schema(tokens: &[Token], schema: &mut Schema) {
    if !tokens
        .first()
        .is_some_and(|token| token_is_word(token, "WITH"))
    {
        return;
    }

    let mut index = 1usize;
    if tokens
        .get(index)
        .is_some_and(|token| token_is_word(token, "RECURSIVE"))
    {
        index += 1;
    }

    while index < tokens.len() {
        let Some(name) = tokens.get(index).and_then(identifier_from_token) else {
            break;
        };
        let cte_name = name.to_ascii_lowercase();
        index += 1;

        let mut column_names = Vec::new();
        if matches!(tokens.get(index), Some(Token::OpenParen)) {
            let (column_tokens, after_columns) = collect_balanced_parens(tokens, index);
            column_names = split_top_level_commas(column_tokens)
                .into_iter()
                .filter_map(|tokens| tokens.first().and_then(identifier_from_token))
                .map(|name| name.to_ascii_lowercase())
                .collect();
            index = after_columns;
        }

        if !tokens
            .get(index)
            .is_some_and(|token| token_is_word(token, "AS"))
            || !matches!(tokens.get(index + 1), Some(Token::OpenParen))
        {
            break;
        }

        let (body, after_body) = collect_balanced_parens(tokens, index + 1);
        if let Some(columns) = infer_cte_columns(body, &column_names, schema) {
            schema.tables.insert(cte_name, columns);
        }
        index = after_body;

        if matches!(tokens.get(index), Some(Token::Comma)) {
            index += 1;
            continue;
        }
        break;
    }
}

fn infer_cte_columns(
    body_tokens: &[Token],
    column_names: &[String],
    schema: &Schema,
) -> Option<BTreeMap<String, SchemaColumn>> {
    let select_list = first_select_list(body_tokens)?;
    let table_refs = table_references(body_tokens);
    let nullable_tables = BTreeSet::new();
    let context = ExpressionContext {
        schema,
        table_refs: &table_refs,
        nullable_tables: &nullable_tables,
    };
    let mut columns = BTreeMap::new();

    for (ordinal, expression) in split_top_level_commas(select_list).into_iter().enumerate() {
        let expression_without_alias = expression_without_alias(expression);
        let name = column_names
            .get(ordinal)
            .cloned()
            .or_else(|| expression_alias(expression).map(|alias| alias.name.to_ascii_lowercase()))
            .or_else(|| {
                column_ref_from_expression(expression_without_alias)
                    .map(|column_ref| column_ref.column.to_ascii_lowercase())
            })?;

        let inference = infer_expression_tokens(expression_without_alias, &context);
        let column_type = inference
            .as_ref()
            .and_then(|inference| inference.column_type.clone())
            .unwrap_or(ValueType::Value);
        let nullable = inference
            .as_ref()
            .and_then(|inference| inference.inferred_nullable)
            .unwrap_or(true);

        columns.insert(
            name,
            SchemaColumn {
                declared_type: declared_type_for_value_type(&column_type).to_string(),
                nullable,
                notnull: !nullable,
                primary_key: false,
                rowid_alias: false,
                ordinal: ordinal as i64,
            },
        );
    }

    Some(columns)
}

fn first_select_list(tokens: &[Token]) -> Option<&[Token]> {
    let mut depth = 0usize;
    let mut select_start = None;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word)
                if depth == 0 && select_start.is_none() && word.eq_ignore_ascii_case("SELECT") =>
            {
                select_start = Some(index + 1);
            }
            Token::Word(word)
                if depth == 0
                    && select_start.is_some()
                    && matches!(
                        word.to_ascii_uppercase().as_str(),
                        "FROM" | "UNION" | "INTERSECT" | "EXCEPT"
                    ) =>
            {
                return select_start.map(|start| &tokens[start..index]);
            }
            _ => {}
        }
    }

    select_start.map(|start| &tokens[start..])
}

fn declared_type_for_value_type(value_type: &ValueType) -> &'static str {
    match value_type {
        ValueType::I64 => "integer",
        ValueType::F64 => "real",
        ValueType::Bool => "boolean",
        ValueType::String => "text",
        ValueType::Bytes => "blob",
        ValueType::Value => "",
    }
}

fn validate_insert_values_counts(sql: &str, schema: &Schema, path: &std::path::Path) -> Result<()> {
    let tokens = tokenize(sql);
    let Some((table, columns, values_index)) = insert_values_shape(&tokens, schema) else {
        return Ok(());
    };
    if !schema.has_table(&table) {
        return Ok(());
    }

    let expected = columns.len();
    for (row_index, row) in values_rows(&tokens, values_index + 1)
        .into_iter()
        .enumerate()
    {
        let got = split_top_level_commas(row).len();
        if got != expected {
            return Err(Error::InsertValuesCountMismatch {
                path: path.to_path_buf(),
                expected,
                got,
                row: row_index + 1,
            });
        }
    }

    Ok(())
}

fn insert_values_shape(tokens: &[Token], schema: &Schema) -> Option<(String, Vec<String>, usize)> {
    let into_index = insert_or_replace_into_index(tokens)?;
    let table = tokens
        .get(into_index + 1)
        .and_then(table_name_from_token)?
        .to_string();
    let (columns, after_target) = insert_target_columns(tokens, schema, &table, into_index);
    let values_index = tokens
        .iter()
        .enumerate()
        .skip(after_target)
        .find_map(|(index, token)| token_is_word(token, "VALUES").then_some(index))?;
    Some((table, columns, values_index))
}

fn table_without_rowid(conn: &Connection, table_name: &str) -> Result<bool> {
    conn.query_row(
        "select wr from pragma_table_list where name = ?1",
        [table_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|without_rowid| without_rowid != 0)
    .map_err(|source| Error::InspectDatabase { source })
}

fn parameters(sql: &str, schema: &Schema) -> Vec<Parameter> {
    let inferences = parameter_inferences(sql, schema);
    let mut params: Vec<Parameter> = Vec::new();
    let mut slots = ParameterSlots::default();

    for token in tokenize(sql) {
        match token {
            Token::ParamNamed { prefix, name } => {
                let sql_name = format!("{prefix}{name}");
                slots.key(&Token::ParamNamed {
                    prefix,
                    name: name.clone(),
                });
                let inference = inferences
                    .get(&sql_name)
                    .cloned()
                    .unwrap_or_else(ParameterInference::default);
                add_named_parameter(&mut params, &name, &sql_name, inference);
            }
            Token::ParamAnon => {
                let Some(placeholder) = slots.key(&Token::ParamAnon) else {
                    continue;
                };
                let slot = placeholder
                    .strip_prefix('?')
                    .and_then(|number| number.parse::<usize>().ok())
                    .unwrap_or(0);
                let name = anonymous_parameter_name(slot);
                let inference = inferences
                    .get(&placeholder)
                    .cloned()
                    .unwrap_or_else(ParameterInference::default);
                params.push(Parameter {
                    name,
                    sql_names: vec![],
                    column_type: inference.column_type,
                    nullable: inference.nullable,
                });
            }
            Token::ParamNumbered { number } => {
                let name = numbered_parameter_name(&number);
                let Some(placeholder) = slots.key(&Token::ParamNumbered {
                    number: number.clone(),
                }) else {
                    continue;
                };
                let sql_name = format!("?{number}");
                let inference = inferences
                    .get(&placeholder)
                    .cloned()
                    .unwrap_or_else(ParameterInference::default);
                if let Some(param) = params
                    .iter_mut()
                    .find(|param| parameter_matches_positional_index(param, &number))
                {
                    if !param.sql_names.contains(&sql_name) {
                        param.sql_names.push(sql_name);
                    }
                } else {
                    let name = unique_parameter_name(&name, &params);
                    params.push(Parameter {
                        name,
                        sql_names: vec![sql_name],
                        column_type: inference.column_type,
                        nullable: inference.nullable,
                    });
                }
            }
            _ => {}
        }
    }

    if params.iter().all(|param| {
        param
            .sql_names
            .iter()
            .all(|sql_name| sql_name.starts_with('?'))
    }) {
        params.sort_by_key(parameter_positional_slot);
    }

    params
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParameterInference {
    column_type: ValueType,
    nullable: bool,
}

impl Default for ParameterInference {
    fn default() -> Self {
        Self {
            column_type: ValueType::String,
            nullable: false,
        }
    }
}

fn add_named_parameter(
    params: &mut Vec<Parameter>,
    raw_name: &str,
    sql_name: &str,
    inference: ParameterInference,
) {
    let logical_name = raw_name.to_ascii_lowercase();
    let name = sanitize_identifier(&raw_name.to_snake_case());
    let sql_name = sql_name.to_string();
    if let Some(param) = params
        .iter_mut()
        .find(|param| parameter_matches_logical_name(param, &logical_name))
    {
        if !param.sql_names.contains(&sql_name) {
            param.sql_names.push(sql_name);
        }
    } else {
        let name = unique_parameter_name(&name, params);
        params.push(Parameter {
            name,
            sql_names: vec![sql_name],
            column_type: inference.column_type,
            nullable: inference.nullable,
        });
    }
}

fn parameter_matches_logical_name(param: &Parameter, logical_name: &str) -> bool {
    param
        .sql_names
        .iter()
        .filter_map(|sql_name| sql_name.get(1..))
        .any(|name| name.to_ascii_lowercase() == logical_name)
}

fn parameter_matches_positional_index(param: &Parameter, number: &str) -> bool {
    let index = numbered_parameter_index(number);
    if param
        .sql_names
        .iter()
        .any(|sql_name| !sql_name.starts_with('?'))
    {
        return false;
    }
    anonymous_parameter_slot(&param.name) == Some(index)
        || param.sql_names.iter().any(|sql_name| {
            sql_name
                .strip_prefix('?')
                .is_some_and(|number| numbered_parameter_index(number) == index)
        })
}

fn unique_parameter_name(base: &str, params: &[Parameter]) -> String {
    if !params.iter().any(|param| param.name == base) {
        return base.to_string();
    }

    for suffix in 2usize.. {
        let candidate = format!("{base}_{suffix}");
        if !params.iter().any(|param| param.name == candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded suffix loop should return")
}

fn anonymous_parameter_name(index: usize) -> String {
    if index == 1 {
        "param".to_string()
    } else {
        format!("param_{index}")
    }
}

fn anonymous_placeholder_key(index: usize) -> String {
    format!("?{index}")
}

fn numbered_parameter_name(number: &str) -> String {
    anonymous_parameter_name(numbered_parameter_index(number))
}

fn numbered_parameter_index(number: &str) -> usize {
    number.parse::<usize>().unwrap_or(0)
}

fn parameter_positional_slot(param: &Parameter) -> usize {
    param
        .sql_names
        .iter()
        .filter_map(|sql_name| {
            sql_name
                .strip_prefix('?')
                .and_then(|number| number.parse::<usize>().ok())
        })
        .min()
        .or_else(|| anonymous_parameter_slot(&param.name))
        .unwrap_or(usize::MAX)
}

fn anonymous_parameter_slot(name: &str) -> Option<usize> {
    if name == "param" {
        return Some(1);
    }
    name.strip_prefix("param_")
        .and_then(|number| number.parse::<usize>().ok())
}

#[derive(Debug, Default)]
struct ParameterSlots {
    next_index: usize,
    named_slots: BTreeMap<String, usize>,
}

impl ParameterSlots {
    fn key(&mut self, token: &Token) -> Option<String> {
        match token {
            Token::ParamNamed { prefix, name } => {
                let sql_name = format!("{prefix}{name}");
                if !self.named_slots.contains_key(&sql_name) {
                    self.next_index += 1;
                    self.named_slots.insert(sql_name.clone(), self.next_index);
                }
                Some(sql_name)
            }
            Token::ParamNumbered { number } => {
                let index = numbered_parameter_index(number);
                self.next_index = self.next_index.max(index);
                Some(anonymous_placeholder_key(index))
            }
            Token::ParamAnon => {
                self.next_index += 1;
                Some(anonymous_placeholder_key(self.next_index))
            }
            _ => None,
        }
    }
}

fn parameter_inferences(sql: &str, schema: &Schema) -> BTreeMap<String, ParameterInference> {
    let tokens = tokenize(sql);
    let schema = schema_with_ctes(&tokens, schema);
    let mut inferences = insert_parameter_inferences(&tokens, &schema);
    let table_refs = table_references(&tokens);

    for (key, inference) in cast_parameter_inferences(&tokens) {
        inferences.insert(key, inference);
    }
    for (key, inference) in comparison_parameter_inferences(&tokens, &schema, &table_refs) {
        inferences.insert(key, inference);
    }
    for (key, inference) in case_result_parameter_inferences(&tokens, &schema, &table_refs) {
        inferences.insert(key, inference);
    }
    for (key, inference) in in_list_parameter_inferences(&tokens, &schema, &table_refs) {
        inferences.insert(key, inference);
    }
    for (key, inference) in between_parameter_inferences(&tokens, &schema, &table_refs) {
        inferences.insert(key, inference);
    }
    for (key, inference) in limit_parameter_inferences(&tokens) {
        inferences.insert(key, inference);
    }
    for (key, inference) in update_set_parameter_inferences(&tokens, &schema, &table_refs) {
        inferences.insert(key, inference);
    }

    inferences
}

fn insert_parameter_inferences(
    tokens: &[Token],
    schema: &Schema,
) -> BTreeMap<String, ParameterInference> {
    let mut inferences = BTreeMap::new();
    let Some(into_index) = insert_or_replace_into_index(tokens) else {
        return inferences;
    };
    let Some(table) = tokens.get(into_index + 1).and_then(table_name_from_token) else {
        return inferences;
    };

    let (columns, after_target) = insert_target_columns(tokens, schema, table, into_index);

    let Some(values_index) = tokens
        .iter()
        .enumerate()
        .skip(after_target)
        .find_map(|(index, token)| token_is_word(token, "VALUES").then_some(index))
    else {
        return insert_select_parameter_inferences(tokens, schema, table, &columns, after_target);
    };
    let Some(Token::OpenParen) = tokens.get(values_index + 1) else {
        return inferences;
    };

    let mut slots = ParameterSlots::default();

    for value_tokens in values_rows(tokens, values_index + 1) {
        let values = split_top_level_commas(value_tokens);
        for (value, column) in values.into_iter().zip(&columns) {
            for token in value {
                let Some(key) = slots.key(token) else {
                    continue;
                };
                if let Some(schema_column) = schema.column(table, column) {
                    inferences.insert(
                        key,
                        ParameterInference {
                            column_type: ValueType::from_sqlite_type(&schema_column.declared_type),
                            nullable: schema_column.nullable || schema_column.rowid_alias,
                        },
                    );
                }
            }
        }
    }

    inferences
}

fn insert_select_parameter_inferences(
    tokens: &[Token],
    schema: &Schema,
    table: &str,
    columns: &[String],
    after_target: usize,
) -> BTreeMap<String, ParameterInference> {
    let mut inferences = BTreeMap::new();
    let Some(select_index) = top_level_keyword_from(tokens, "SELECT", after_target) else {
        return inferences;
    };
    let select_list_end =
        top_level_keyword_from(tokens, "FROM", select_index + 1).unwrap_or(tokens.len());
    let parameter_keys = parameter_keys_by_index(tokens);

    for ((start, end), column) in
        split_top_level_comma_ranges(tokens, select_index + 1, select_list_end)
            .into_iter()
            .zip(columns)
    {
        let Some(schema_column) = schema.column(table, column) else {
            continue;
        };
        for token_index in start..end {
            let Some(key) = parameter_keys.get(&token_index) else {
                continue;
            };
            inferences.insert(
                key.clone(),
                ParameterInference {
                    column_type: ValueType::from_sqlite_type(&schema_column.declared_type),
                    nullable: schema_column.nullable || schema_column.rowid_alias,
                },
            );
        }
    }

    inferences
}

fn top_level_keyword_from(tokens: &[Token], keyword: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case(keyword) => {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_comma_ranges(tokens: &[Token], start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut expression_start = start;

    for (index, token) in tokens.iter().enumerate().take(end).skip(start) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                ranges.push((expression_start, index));
                expression_start = index + 1;
            }
            _ => {}
        }
    }

    if expression_start < end {
        ranges.push((expression_start, end));
    }
    ranges
}

fn insert_target_columns(
    tokens: &[Token],
    schema: &Schema,
    table: &str,
    into_index: usize,
) -> (Vec<String>, usize) {
    if matches!(tokens.get(into_index + 2), Some(Token::OpenParen)) {
        let (column_tokens, after_columns) = collect_balanced_parens(tokens, into_index + 2);
        let columns = split_top_level_commas(column_tokens)
            .into_iter()
            .filter_map(|tokens| tokens.first().and_then(identifier_from_token))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        (columns, after_columns)
    } else {
        (schema.column_names_in_table_order(table), into_index + 2)
    }
}

fn values_rows(tokens: &[Token], mut index: usize) -> Vec<&[Token]> {
    let mut rows = Vec::new();
    while index < tokens.len() {
        if matches!(tokens.get(index), Some(Token::Comma)) {
            index += 1;
            continue;
        }
        if !matches!(tokens.get(index), Some(Token::OpenParen)) {
            break;
        }

        let (row, after_row) = collect_balanced_parens(tokens, index);
        rows.push(row);
        index = after_row;

        if !matches!(tokens.get(index), Some(Token::Comma)) {
            break;
        }
    }
    rows
}

fn insert_or_replace_into_index(tokens: &[Token]) -> Option<usize> {
    if let Some(insert_index) = top_level_keyword(tokens, "INSERT") {
        return insert_into_index(tokens, insert_index);
    }

    if let Some(replace_index) = top_level_keyword(tokens, "REPLACE") {
        if tokens
            .get(replace_index + 1)
            .is_some_and(|token| token_is_word(token, "INTO"))
        {
            return Some(replace_index + 1);
        }
    }

    None
}

fn insert_into_index(tokens: &[Token], insert_index: usize) -> Option<usize> {
    if tokens
        .get(insert_index + 1)
        .is_some_and(|token| token_is_word(token, "INTO"))
    {
        return Some(insert_index + 1);
    }

    let has_conflict_action = tokens
        .get(insert_index + 1)
        .is_some_and(|token| token_is_word(token, "OR"))
        && tokens.get(insert_index + 2).is_some_and(|token| {
            matches!(
                identifier_from_token(token).map(|word| word.to_ascii_uppercase()),
                Some(action)
                    if matches!(
                        action.as_str(),
                        "ABORT" | "FAIL" | "IGNORE" | "REPLACE" | "ROLLBACK"
                    )
            )
        });
    if has_conflict_action
        && tokens
            .get(insert_index + 3)
            .is_some_and(|token| token_is_word(token, "INTO"))
    {
        return Some(insert_index + 3);
    }

    None
}

fn cast_parameter_inferences(tokens: &[Token]) -> BTreeMap<String, ParameterInference> {
    let keys_by_index = parameter_keys_by_index(tokens);
    let mut inferences = BTreeMap::new();
    let mut index = 0usize;

    while index < tokens.len() {
        if !token_is_word(&tokens[index], "CAST")
            || !matches!(tokens.get(index + 1), Some(Token::OpenParen))
        {
            index += 1;
            continue;
        }

        let (inside, after_cast) = collect_balanced_parens(tokens, index + 1);
        let Some(as_index) = inside.iter().position(|token| token_is_word(token, "AS")) else {
            index = after_cast;
            continue;
        };
        let Some(declared_type) = inside.get(as_index + 1).and_then(identifier_from_token) else {
            index = after_cast;
            continue;
        };

        for token_index in index + 2..after_cast.saturating_sub(1) {
            if let Some(key) = keys_by_index.get(&token_index) {
                inferences.insert(
                    key.clone(),
                    ParameterInference {
                        column_type: ValueType::from_sqlite_type(declared_type),
                        nullable: false,
                    },
                );
            }
        }

        index = after_cast;
    }

    inferences
}

fn parameter_keys_by_index(tokens: &[Token]) -> BTreeMap<usize, String> {
    let mut keys = BTreeMap::new();
    let mut slots = ParameterSlots::default();
    for (index, token) in tokens.iter().enumerate() {
        if let Some(key) = slots.key(token) {
            keys.insert(index, key);
        }
    }
    keys
}

fn comparison_parameter_inferences(
    tokens: &[Token],
    schema: &Schema,
    table_refs: &BTreeMap<String, String>,
) -> BTreeMap<String, ParameterInference> {
    let mut inferences = BTreeMap::new();
    let mut slots = ParameterSlots::default();

    for (index, token) in tokens.iter().enumerate() {
        let Some(key) = slots.key(token) else {
            continue;
        };

        let column_before = comparison_operator_before(tokens, index)
            .and_then(|column_end| comparison_column_ref_ending_at(tokens, column_end));
        let column_after = comparison_operator_after(tokens, index)
            .and_then(|column_start| comparison_column_ref_starting_at(tokens, column_start));
        let arithmetic_column = arithmetic_column_for_parameter(tokens, index);

        let Some(column_ref) = column_before.or(column_after).or(arithmetic_column) else {
            continue;
        };
        let Some(schema_column) = resolve_column_ref(schema, table_refs, &column_ref) else {
            continue;
        };

        inferences.insert(
            key,
            ParameterInference {
                column_type: ValueType::from_sqlite_type(&schema_column.declared_type),
                nullable: false,
            },
        );
    }

    inferences
}

fn in_list_parameter_inferences(
    tokens: &[Token],
    schema: &Schema,
    table_refs: &BTreeMap<String, String>,
) -> BTreeMap<String, ParameterInference> {
    let parameter_keys = parameter_keys_by_index(tokens);
    let mut inferences = BTreeMap::new();

    for (index, token) in tokens.iter().enumerate() {
        if !token_is_word(token, "IN") {
            continue;
        }
        let Some(Token::OpenParen) = tokens.get(index + 1) else {
            continue;
        };
        let Some(column_ref) = in_list_column_ref(tokens, index) else {
            continue;
        };
        let Some(schema_column) = resolve_column_ref(schema, table_refs, &column_ref) else {
            continue;
        };
        let (inside, after_list) = collect_balanced_parens(tokens, index + 1);
        if inside
            .iter()
            .any(|token| token_is_word(token, "SELECT") || token_is_word(token, "WITH"))
        {
            continue;
        }

        for token_index in index + 2..after_list.saturating_sub(1) {
            let Some(key) = parameter_keys.get(&token_index) else {
                continue;
            };
            inferences.insert(
                key.clone(),
                ParameterInference {
                    column_type: ValueType::from_sqlite_type(&schema_column.declared_type),
                    nullable: false,
                },
            );
        }
    }

    inferences
}

fn in_list_column_ref(tokens: &[Token], in_index: usize) -> Option<ColumnRef> {
    if tokens
        .get(in_index.checked_sub(1)?)
        .is_some_and(|token| token_is_word(token, "NOT"))
    {
        return column_ref_ending_at(tokens, in_index.checked_sub(2)?);
    }
    column_ref_ending_at(tokens, in_index.checked_sub(1)?)
}

fn case_result_parameter_inferences(
    tokens: &[Token],
    schema: &Schema,
    table_refs: &BTreeMap<String, String>,
) -> BTreeMap<String, ParameterInference> {
    let parameter_keys = parameter_keys_by_index(tokens);
    let mut inferences = BTreeMap::new();
    let mut index = 0usize;

    while index < tokens.len() {
        if !token_is_word(&tokens[index], "CASE") {
            index += 1;
            continue;
        }
        let Some(end_index) = matching_case_end(tokens, index) else {
            index += 1;
            continue;
        };
        let Some(column_ref) = compared_column_for_case(tokens, index, end_index) else {
            index = end_index + 1;
            continue;
        };
        let Some(schema_column) = resolve_column_ref(schema, table_refs, &column_ref) else {
            index = end_index + 1;
            continue;
        };

        for token_index in case_result_parameter_indexes(tokens, index, end_index) {
            let Some(key) = parameter_keys.get(&token_index) else {
                continue;
            };
            inferences.insert(
                key.clone(),
                ParameterInference {
                    column_type: ValueType::from_sqlite_type(&schema_column.declared_type),
                    nullable: false,
                },
            );
        }

        index = end_index + 1;
    }

    inferences
}

fn compared_column_for_case(
    tokens: &[Token],
    case_index: usize,
    end_index: usize,
) -> Option<ColumnRef> {
    if tokens.get(end_index + 1).is_some_and(comparison_operator) {
        return comparison_column_ref_starting_at(tokens, end_index + 2);
    }
    if tokens
        .get(case_index.checked_sub(1)?)
        .is_some_and(comparison_operator)
    {
        return comparison_column_ref_ending_at(tokens, case_index.checked_sub(2)?);
    }
    None
}

fn case_result_parameter_indexes(
    tokens: &[Token],
    case_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let mut indexes = Vec::new();
    let mut in_result = false;
    let mut paren_depth = 0usize;
    let mut nested_case_depth = 0usize;

    for index in case_index + 1..end_index {
        match &tokens[index] {
            Token::OpenParen => paren_depth += 1,
            Token::CloseParen => paren_depth = paren_depth.saturating_sub(1),
            Token::Word(word) if paren_depth == 0 && word.eq_ignore_ascii_case("CASE") => {
                nested_case_depth += 1;
            }
            Token::Word(word)
                if paren_depth == 0
                    && nested_case_depth > 0
                    && word.eq_ignore_ascii_case("END") =>
            {
                nested_case_depth = nested_case_depth.saturating_sub(1);
            }
            Token::Word(word)
                if paren_depth == 0
                    && nested_case_depth == 0
                    && word.eq_ignore_ascii_case("WHEN") =>
            {
                in_result = false;
            }
            Token::Word(word)
                if paren_depth == 0
                    && nested_case_depth == 0
                    && (word.eq_ignore_ascii_case("THEN") || word.eq_ignore_ascii_case("ELSE")) =>
            {
                in_result = true;
            }
            Token::ParamAnon | Token::ParamNumbered { .. } | Token::ParamNamed { .. }
                if in_result =>
            {
                indexes.push(index);
            }
            _ => {}
        }
    }

    indexes
}

fn matching_case_end(tokens: &[Token], case_index: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(case_index) {
        if token_is_word(token, "CASE") {
            depth += 1;
        } else if token_is_word(token, "END") {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }

    None
}

fn arithmetic_column_for_parameter(tokens: &[Token], param_index: usize) -> Option<ColumnRef> {
    if tokens
        .get(param_index.checked_sub(1)?)
        .is_some_and(arithmetic_operator)
    {
        return column_ref_ending_at(tokens, param_index.checked_sub(2)?);
    }

    if tokens.get(param_index + 1).is_some_and(arithmetic_operator) {
        return column_ref_starting_at(tokens, param_index + 2);
    }

    None
}

fn arithmetic_operator(token: &Token) -> bool {
    matches!(
        token,
        Token::Plus | Token::Minus | Token::Star | Token::Slash | Token::Percent
    )
}

fn limit_parameter_inferences(tokens: &[Token]) -> BTreeMap<String, ParameterInference> {
    let mut inferences = BTreeMap::new();
    let mut slots = ParameterSlots::default();

    for (index, token) in tokens.iter().enumerate() {
        let key = slots.key(token);
        if !tokens
            .get(index.saturating_sub(1))
            .is_some_and(|token| token_is_word(token, "LIMIT") || token_is_word(token, "OFFSET"))
        {
            continue;
        }
        if let Some(key) = key {
            inferences.insert(
                key,
                ParameterInference {
                    column_type: ValueType::I64,
                    nullable: false,
                },
            );
        }
    }

    inferences
}

fn between_parameter_inferences(
    tokens: &[Token],
    schema: &Schema,
    table_refs: &BTreeMap<String, String>,
) -> BTreeMap<String, ParameterInference> {
    let mut inferences = BTreeMap::new();
    let mut slots = ParameterSlots::default();

    for (index, token) in tokens.iter().enumerate() {
        let key = slots.key(token);
        let Some(key) = key else {
            continue;
        };

        let Some(column_ref) = between_column_for_parameter(tokens, index) else {
            continue;
        };
        let Some(schema_column) = resolve_column_ref(schema, table_refs, &column_ref) else {
            continue;
        };

        inferences.insert(
            key,
            ParameterInference {
                column_type: ValueType::from_sqlite_type(&schema_column.declared_type),
                nullable: false,
            },
        );
    }

    inferences
}

fn update_set_parameter_inferences(
    tokens: &[Token],
    schema: &Schema,
    table_refs: &BTreeMap<String, String>,
) -> BTreeMap<String, ParameterInference> {
    let mut inferences = BTreeMap::new();
    let Some(set_start) = top_level_keyword(tokens, "SET").map(|index| index + 1) else {
        return inferences;
    };
    let set_end = update_set_clause_end(tokens, set_start);
    let parameter_keys = parameter_keys_by_index(tokens);
    let mut assignment_start = set_start;
    let mut depth = 0usize;

    for index in set_start..=set_end {
        let token = tokens.get(index);
        match token {
            Some(Token::OpenParen) => depth += 1,
            Some(Token::CloseParen) => depth = depth.saturating_sub(1),
            Some(Token::Comma) if depth == 0 => {
                infer_update_assignment_parameters(
                    tokens,
                    assignment_start,
                    index,
                    schema,
                    table_refs,
                    &parameter_keys,
                    &mut inferences,
                );
                assignment_start = index + 1;
            }
            None if assignment_start < set_end => {
                infer_update_assignment_parameters(
                    tokens,
                    assignment_start,
                    set_end,
                    schema,
                    table_refs,
                    &parameter_keys,
                    &mut inferences,
                );
            }
            _ if index == set_end && assignment_start < set_end => {
                infer_update_assignment_parameters(
                    tokens,
                    assignment_start,
                    set_end,
                    schema,
                    table_refs,
                    &parameter_keys,
                    &mut inferences,
                );
            }
            _ => {}
        }
    }

    inferences
}

fn update_set_clause_end(tokens: &[Token], start: usize) -> usize {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word)
                if depth == 0
                    && matches!(
                        word.to_ascii_uppercase().as_str(),
                        "FROM" | "WHERE" | "ORDER" | "LIMIT" | "RETURNING"
                    ) =>
            {
                return index;
            }
            _ => {}
        }
    }
    tokens.len()
}

fn infer_update_assignment_parameters(
    tokens: &[Token],
    start: usize,
    end: usize,
    schema: &Schema,
    table_refs: &BTreeMap<String, String>,
    parameter_keys: &BTreeMap<usize, String>,
    inferences: &mut BTreeMap<String, ParameterInference>,
) {
    let Some(eq_index) = top_level_eq_index(&tokens[start..end]).map(|index| start + index) else {
        return;
    };
    let Some(column_ref) = column_ref_ending_at(tokens, eq_index.saturating_sub(1)) else {
        return;
    };
    let Some(schema_column) = resolve_column_ref(schema, table_refs, &column_ref) else {
        return;
    };

    for token_index in eq_index + 1..end {
        let Some(key) = parameter_keys.get(&token_index) else {
            continue;
        };
        inferences.insert(
            key.clone(),
            ParameterInference {
                column_type: ValueType::from_sqlite_type(&schema_column.declared_type),
                nullable: schema_column.nullable,
            },
        );
    }
}

fn top_level_eq_index(tokens: &[Token]) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Eq if depth == 0 => return Some(index),
            _ => {}
        }
    }

    None
}

fn between_column_for_parameter(tokens: &[Token], param_index: usize) -> Option<ColumnRef> {
    for between_index in (0..param_index).rev() {
        if !token_is_word(&tokens[between_index], "BETWEEN") {
            continue;
        }
        let column_end = if between_index >= 2 && token_is_word(&tokens[between_index - 1], "NOT") {
            between_index - 2
        } else {
            between_index.checked_sub(1)?
        };
        return column_ref_ending_at(tokens, column_end);
    }

    None
}

fn collect_balanced_parens(tokens: &[Token], open_index: usize) -> (&[Token], usize) {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open_index) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (&tokens[open_index + 1..index], index + 1);
                }
            }
            _ => {}
        }
    }

    (&tokens[open_index + 1..], tokens.len())
}

fn comparison_operator_before(tokens: &[Token], param_index: usize) -> Option<usize> {
    let previous = tokens.get(param_index.checked_sub(1)?)?;
    if comparison_operator(previous) {
        return param_index.checked_sub(2);
    }
    if token_is_word(previous, "LIKE") {
        if tokens
            .get(param_index.checked_sub(2)?)
            .is_some_and(|token| token_is_word(token, "NOT"))
        {
            return param_index.checked_sub(3);
        }
        return param_index.checked_sub(2);
    }
    if token_is_word(previous, "NOT")
        && tokens
            .get(param_index.checked_sub(2)?)
            .is_some_and(|token| token_is_word(token, "IS"))
    {
        return param_index.checked_sub(3);
    }
    if token_is_word(previous, "IS") {
        return param_index.checked_sub(2);
    }
    None
}

fn comparison_operator_after(tokens: &[Token], param_index: usize) -> Option<usize> {
    let next = tokens.get(param_index + 1)?;
    if comparison_operator(next) {
        return Some(param_index + 2);
    }
    if token_is_word(next, "LIKE") {
        return Some(param_index + 2);
    }
    if token_is_word(next, "NOT")
        && tokens
            .get(param_index + 2)
            .is_some_and(|token| token_is_word(token, "LIKE"))
    {
        return Some(param_index + 3);
    }
    if token_is_word(next, "IS") {
        if tokens
            .get(param_index + 2)
            .is_some_and(|token| token_is_word(token, "NOT"))
        {
            return Some(param_index + 3);
        }
        return Some(param_index + 2);
    }
    None
}

fn comparison_operator(token: &Token) -> bool {
    matches!(
        token,
        Token::Eq | Token::Ne | Token::Lt | Token::Gt | Token::Le | Token::Ge
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnRef {
    qualifier: Option<String>,
    column: String,
}

fn column_ref_ending_at(tokens: &[Token], index: usize) -> Option<ColumnRef> {
    let column = identifier_from_token(tokens.get(index)?)?;
    if index >= 2 && matches!(tokens.get(index - 1), Some(Token::Dot)) {
        let qualifier = identifier_from_token(tokens.get(index - 2)?)?;
        Some(ColumnRef {
            qualifier: Some(qualifier.to_string()),
            column: column.to_string(),
        })
    } else {
        Some(ColumnRef {
            qualifier: None,
            column: column.to_string(),
        })
    }
}

fn column_ref_starting_at(tokens: &[Token], index: usize) -> Option<ColumnRef> {
    let first = identifier_from_token(tokens.get(index)?)?;
    if matches!(tokens.get(index + 1), Some(Token::Dot)) {
        let column = identifier_from_token(tokens.get(index + 2)?)?;
        Some(ColumnRef {
            qualifier: Some(first.to_string()),
            column: column.to_string(),
        })
    } else {
        Some(ColumnRef {
            qualifier: None,
            column: first.to_string(),
        })
    }
}

fn comparison_column_ref_ending_at(tokens: &[Token], index: usize) -> Option<ColumnRef> {
    column_ref_ending_at(tokens, index).or_else(|| aggregate_column_ref_ending_at(tokens, index))
}

fn comparison_column_ref_starting_at(tokens: &[Token], index: usize) -> Option<ColumnRef> {
    column_ref_starting_at(tokens, index)
}

fn aggregate_column_ref_ending_at(tokens: &[Token], close_paren_index: usize) -> Option<ColumnRef> {
    if !matches!(tokens.get(close_paren_index), Some(Token::CloseParen)) {
        return None;
    }

    let open_paren_index = matching_open_paren(tokens, close_paren_index)?;
    let function_name = identifier_from_token(tokens.get(open_paren_index.checked_sub(1)?)?)?;
    if !matches!(
        function_name.to_ascii_uppercase().as_str(),
        "AVG" | "MAX" | "MIN" | "SUM" | "TOTAL"
    ) {
        return None;
    }

    let inside = &tokens[open_paren_index + 1..close_paren_index];
    let expressions = split_top_level_commas(inside);
    let [expression] = expressions.as_slice() else {
        return None;
    };
    match *expression {
        [Token::Word(_)] | [Token::QuotedId(_)] => column_ref_starting_at(expression, 0),
        [Token::Word(_), Token::Dot, Token::Word(_)]
        | [Token::Word(_), Token::Dot, Token::QuotedId(_)]
        | [Token::QuotedId(_), Token::Dot, Token::Word(_)]
        | [Token::QuotedId(_), Token::Dot, Token::QuotedId(_)] => {
            column_ref_starting_at(expression, 0)
        }
        _ => None,
    }
}

fn matching_open_paren(tokens: &[Token], close_paren_index: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().take(close_paren_index + 1).rev() {
        match token {
            Token::CloseParen => depth += 1,
            Token::OpenParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }

    None
}

fn resolve_column_ref<'a>(
    schema: &'a Schema,
    table_refs: &BTreeMap<String, String>,
    column_ref: &ColumnRef,
) -> Option<&'a SchemaColumn> {
    resolve_column_ref_with_table(schema, table_refs, column_ref).map(|(_, column)| column)
}

fn resolve_column_ref_with_table<'a>(
    schema: &'a Schema,
    table_refs: &BTreeMap<String, String>,
    column_ref: &ColumnRef,
) -> Option<(String, &'a SchemaColumn)> {
    if let Some(qualifier) = &column_ref.qualifier {
        return table_refs
            .get(&qualifier.to_ascii_lowercase())
            .and_then(|table| {
                schema
                    .column(table, &column_ref.column)
                    .map(|column| (table.clone(), column))
            });
    }

    table_refs
        .values()
        .filter_map(|table| {
            schema
                .column(table, &column_ref.column)
                .map(|column| (table.clone(), column))
        })
        .next()
}

fn table_references(tokens: &[Token]) -> BTreeMap<String, String> {
    let mut refs = crate::sqlite::parse::table_references(tokens);

    match crate::sqlite::parse::parse_statement(tokens) {
        crate::sqlite::parse::Statement::Insert(statement) => {
            add_table_binding_ref(&mut refs, &statement.target);
        }
        crate::sqlite::parse::Statement::Update(statement) => {
            add_table_binding_ref(&mut refs, &statement.target);
        }
        crate::sqlite::parse::Statement::Delete(statement) => {
            add_table_binding_ref(&mut refs, &statement.target);
        }
        _ => {}
    }

    refs
}

fn add_table_binding_ref(
    refs: &mut BTreeMap<String, String>,
    binding: &crate::sqlite::parse::TableBinding,
) {
    let key = binding
        .alias
        .as_deref()
        .unwrap_or(&binding.table.name)
        .to_ascii_lowercase();
    refs.insert(key, binding.table.name.to_ascii_lowercase());
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
    let nullable_tables = outer_join_nullable_tables(sql);
    let tokens = tokenize(sql);
    let schema = schema_with_ctes(&tokens, schema);
    let expression_inferences = select_expression_inferences(sql, &schema, &nullable_tables);

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

fn outer_join_nullable_tables(sql: &str) -> BTreeSet<String> {
    let tokens = tokenize(sql);
    let mut aliases = BTreeMap::new();
    let mut nullable_tables = BTreeSet::new();
    let mut joined_tables = BTreeSet::new();
    let mut index = 0;

    while index < tokens.len() {
        if token_is_word(&tokens[index], "FROM") {
            register_joined_table(&tokens, index + 1, &mut aliases, &mut joined_tables);
            index += 1;
            continue;
        }

        if token_is_word(&tokens[index], "JOIN") {
            register_joined_table(&tokens, index + 1, &mut aliases, &mut joined_tables);
            index += 1;
            continue;
        }

        if token_is_word(&tokens[index], "LEFT") {
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

            if let Some(table_name) =
                register_joined_table(&tokens, join_index + 1, &mut aliases, &mut joined_tables)
            {
                nullable_tables.insert(table_name);
            }

            index = join_index + 1;
            continue;
        }

        if token_is_word(&tokens[index], "RIGHT") {
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

            nullable_tables.extend(joined_tables.iter().cloned());
            register_joined_table(&tokens, join_index + 1, &mut aliases, &mut joined_tables);

            index = join_index + 1;
            continue;
        }

        index += 1;
    }

    for table in where_null_rejected_tables(&tokens, &aliases) {
        nullable_tables.remove(&table);
    }

    nullable_tables
}

fn register_joined_table(
    tokens: &[Token],
    table_index: usize,
    aliases: &mut BTreeMap<String, String>,
    joined_tables: &mut BTreeSet<String>,
) -> Option<String> {
    let table_name = tokens
        .get(table_index)
        .and_then(table_name_from_token)?
        .to_ascii_lowercase();
    aliases.insert(table_name.clone(), table_name.clone());
    if let Some(alias) = table_alias_after_join(tokens, table_index + 1) {
        aliases.insert(alias.to_ascii_lowercase(), table_name.clone());
    }
    joined_tables.insert(table_name.clone());
    Some(table_name)
}

fn table_alias_after_join(tokens: &[Token], mut index: usize) -> Option<&str> {
    if tokens
        .get(index)
        .is_some_and(|token| token_is_word(token, "AS"))
    {
        index += 1;
    }

    let token = tokens.get(index)?;
    let alias = table_name_from_token(token)?;
    (!join_clause_boundary(alias)).then_some(alias)
}

fn join_clause_boundary(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "ON" | "USING" | "JOIN" | "LEFT" | "RIGHT" | "INNER" | "CROSS" | "FULL" | "WHERE"
    )
}

fn where_null_rejected_tables(
    tokens: &[Token],
    aliases: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let Some(where_start) = top_level_keyword(tokens, "WHERE") else {
        return BTreeSet::new();
    };
    let where_end = top_level_clause_end(tokens, where_start + 1);
    let mut rejected = BTreeSet::new();
    let mut index = where_start + 1;

    while index < where_end {
        let Some((qualifier, next_index)) = qualified_column_at(tokens, index) else {
            index += 1;
            continue;
        };

        if null_rejecting_predicate_follows(tokens, next_index, where_end) {
            if let Some(table) = aliases.get(&qualifier.to_ascii_lowercase()) {
                rejected.insert(table.clone());
            }
        }

        index = next_index;
    }

    rejected
}

fn top_level_keyword(tokens: &[Token], keyword: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case(keyword) => {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

fn top_level_clause_end(tokens: &[Token], start: usize) -> usize {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word)
                if depth == 0
                    && matches!(
                        word.to_ascii_uppercase().as_str(),
                        "GROUP" | "HAVING" | "ORDER" | "LIMIT" | "RETURNING"
                    ) =>
            {
                return index;
            }
            _ => {}
        }
    }
    tokens.len()
}

fn qualified_column_at(tokens: &[Token], index: usize) -> Option<(&str, usize)> {
    match (
        tokens.get(index),
        tokens.get(index + 1),
        tokens.get(index + 2),
    ) {
        (Some(Token::Word(qualifier)), Some(Token::Dot), Some(Token::Word(_)))
        | (Some(Token::QuotedId(qualifier)), Some(Token::Dot), Some(Token::Word(_)))
        | (Some(Token::Word(qualifier)), Some(Token::Dot), Some(Token::QuotedId(_)))
        | (Some(Token::QuotedId(qualifier)), Some(Token::Dot), Some(Token::QuotedId(_))) => {
            Some((qualifier.as_str(), index + 3))
        }
        _ => None,
    }
}

fn null_rejecting_predicate_follows(tokens: &[Token], index: usize, end: usize) -> bool {
    match tokens.get(index) {
        Some(Token::Eq | Token::Ne | Token::Lt | Token::Gt | Token::Le | Token::Ge) => true,
        Some(Token::Word(word)) if word.eq_ignore_ascii_case("IS") => {
            tokens
                .get(index + 1)
                .is_some_and(|token| token_is_word(token, "NOT"))
                && tokens
                    .get(index + 2)
                    .is_some_and(|token| token_is_word(token, "NULL"))
                && index + 2 < end
        }
        _ => false,
    }
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

fn select_expression_inferences(
    sql: &str,
    schema: &Schema,
    nullable_tables: &BTreeSet<String>,
) -> BTreeMap<String, ExpressionInference> {
    let tokens = tokenize(sql);
    let Some(expression_list) =
        top_level_select_list(&tokens).or_else(|| top_level_returning_list(&tokens))
    else {
        return BTreeMap::new();
    };
    let table_refs = table_references(&tokens);
    let context = ExpressionContext {
        schema,
        table_refs: &table_refs,
        nullable_tables,
    };
    let mut inferences = BTreeMap::new();

    for expression in split_top_level_commas(expression_list) {
        let alias = expression_alias(expression);
        let expression = expression_without_alias(expression);
        let Some(name) = alias
            .as_ref()
            .map(|alias| alias.name.clone())
            .or_else(|| column_ref_from_expression(expression).map(|column_ref| column_ref.column))
        else {
            continue;
        };
        let expression_inference = infer_expression_tokens(expression, &context);
        let nullable_override = alias.and_then(|alias| alias.nullable_override);
        if expression_inference.is_some() || nullable_override.is_some() {
            let mut inference = expression_inference.unwrap_or(ExpressionInference {
                column_type: None,
                inferred_nullable: None,
                nullable_override: None,
            });
            inference.nullable_override = nullable_override;
            inferences.insert(name.to_ascii_lowercase(), inference);
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

fn top_level_returning_list(tokens: &[Token]) -> Option<&[Token]> {
    let mut depth = 0usize;
    let mut returning_start = None;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case("RETURNING") => {
                returning_start = Some(index + 1);
            }
            Token::Semicolon if depth == 0 => {
                if let Some(start) = returning_start {
                    return Some(&tokens[start..index]);
                }
            }
            _ => {}
        }
    }

    returning_start.map(|start| &tokens[start..])
}

#[derive(Debug)]
struct ExpressionContext<'a> {
    schema: &'a Schema,
    table_refs: &'a BTreeMap<String, String>,
    nullable_tables: &'a BTreeSet<String>,
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

fn infer_expression_tokens(
    tokens: &[Token],
    context: &ExpressionContext<'_>,
) -> Option<ExpressionInference> {
    if let Some(inference) = infer_top_level_numeric_binary_expression(tokens, context) {
        return Some(inference);
    }
    if tokens
        .first()
        .is_some_and(|token| token_is_word(token, "CASE"))
    {
        return infer_case_expression(tokens, context);
    }
    if tokens
        .first()
        .is_some_and(|token| token_is_word(token, "CAST"))
    {
        return infer_cast_expression(tokens);
    }
    if let [Token::Minus | Token::Plus, Token::Number(number)] = tokens {
        return Some(ExpressionInference {
            column_type: Some(number_value_type(number)),
            inferred_nullable: Some(false),
            nullable_override: None,
        });
    }
    if let Some(Token::Number(number)) = tokens.first() {
        return Some(ExpressionInference {
            column_type: Some(number_value_type(number)),
            inferred_nullable: Some(false),
            nullable_override: None,
        });
    }
    if matches!(tokens.first(), Some(Token::StringLit(_))) {
        return Some(ExpressionInference {
            column_type: Some(ValueType::String),
            inferred_nullable: Some(false),
            nullable_override: None,
        });
    }
    if let Some((table, schema_column)) = infer_column_ref_expression(tokens, context) {
        return Some(ExpressionInference {
            column_type: Some(ValueType::from_sqlite_type(&schema_column.declared_type)),
            inferred_nullable: Some(
                schema_column.nullable || context.nullable_tables.contains(&table),
            ),
            nullable_override: None,
        });
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
        "exists" => Some(ExpressionInference {
            column_type: Some(ValueType::I64),
            inferred_nullable: Some(false),
            nullable_override: None,
        }),
        "sum" | "avg" => Some(ExpressionInference {
            column_type: Some(ValueType::F64),
            inferred_nullable: Some(true),
            nullable_override: None,
        }),
        "max" | "min" => infer_min_max_expression(tokens, context),
        "coalesce" => infer_coalesce_expression(tokens, context),
        _ => None,
    }
}

fn infer_top_level_numeric_binary_expression(
    tokens: &[Token],
    context: &ExpressionContext<'_>,
) -> Option<ExpressionInference> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Plus | Token::Minus if depth == 0 && index > 0 => {
                let left = infer_expression_tokens(&tokens[..index], context)?;
                let right = infer_expression_tokens(&tokens[index + 1..], context)?;
                let column_type = common_numeric_type(left.column_type?, right.column_type?)?;
                return Some(ExpressionInference {
                    column_type: Some(column_type),
                    inferred_nullable: Some(
                        left.inferred_nullable.unwrap_or(true)
                            || right.inferred_nullable.unwrap_or(true),
                    ),
                    nullable_override: None,
                });
            }
            _ => {}
        }
    }
    None
}

fn common_numeric_type(left: ValueType, right: ValueType) -> Option<ValueType> {
    match (left, right) {
        (ValueType::F64, ValueType::I64)
        | (ValueType::I64, ValueType::F64)
        | (ValueType::F64, ValueType::F64) => Some(ValueType::F64),
        (ValueType::I64, ValueType::I64) => Some(ValueType::I64),
        _ => None,
    }
}

fn infer_coalesce_expression(
    tokens: &[Token],
    context: &ExpressionContext<'_>,
) -> Option<ExpressionInference> {
    let (inside, _) = collect_balanced_parens(tokens, 1);
    let mut column_type = None;
    let mut nullable = true;

    for arg in split_top_level_commas(inside) {
        let Some(inference) = infer_expression_tokens(arg, context) else {
            continue;
        };
        if column_type.is_none() {
            column_type = inference.column_type;
        }
        if inference.inferred_nullable == Some(false) {
            nullable = false;
        }
    }

    Some(ExpressionInference {
        column_type,
        inferred_nullable: Some(nullable),
        nullable_override: None,
    })
}

fn infer_min_max_expression(
    tokens: &[Token],
    context: &ExpressionContext<'_>,
) -> Option<ExpressionInference> {
    let (inside, _) = collect_balanced_parens(tokens, 1);
    let args = split_top_level_commas(inside);
    let [arg] = args.as_slice() else {
        return None;
    };
    let mut inference = infer_expression_tokens(arg, context)?;
    inference.inferred_nullable = Some(true);
    Some(inference)
}

fn infer_cast_expression(tokens: &[Token]) -> Option<ExpressionInference> {
    if !matches!(tokens.get(1), Some(Token::OpenParen)) {
        return None;
    }
    let (inside, _) = collect_balanced_parens(tokens, 1);
    let as_index = top_level_as_index(inside)?;
    let declared_type = inside.get(as_index + 1).and_then(identifier_from_token)?;

    Some(ExpressionInference {
        column_type: Some(ValueType::from_sqlite_type(declared_type)),
        inferred_nullable: Some(false),
        nullable_override: None,
    })
}

fn top_level_as_index(tokens: &[Token]) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word) if depth == 0 && word.eq_ignore_ascii_case("AS") => {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

fn infer_case_expression(
    tokens: &[Token],
    context: &ExpressionContext<'_>,
) -> Option<ExpressionInference> {
    let mut branch_types = Vec::new();
    let mut nullable = false;
    let mut has_else = false;
    let mut index = 1usize;

    while index < tokens.len() {
        if token_is_word(&tokens[index], "THEN") {
            let start = index + 1;
            let end = case_branch_end(tokens, start);
            let branch = infer_case_branch(&tokens[start..end], context);
            nullable |= branch.nullable;
            if let Some(column_type) = branch.column_type {
                branch_types.push(column_type);
            }
            index = end;
        } else if token_is_word(&tokens[index], "ELSE") {
            has_else = true;
            let start = index + 1;
            let end = case_branch_end(tokens, start);
            let branch = infer_case_branch(&tokens[start..end], context);
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

fn infer_case_branch(tokens: &[Token], context: &ExpressionContext<'_>) -> CaseBranch {
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
        Some(Token::Word(word)) if word.eq_ignore_ascii_case("CASE") => CaseBranch {
            column_type: Some(ValueType::String),
            nullable: true,
        },
        _ => infer_column_ref_case_branch(tokens, context).unwrap_or(CaseBranch {
            column_type: None,
            nullable: true,
        }),
    }
}

fn infer_column_ref_case_branch(
    tokens: &[Token],
    context: &ExpressionContext<'_>,
) -> Option<CaseBranch> {
    let (table, schema_column) = infer_column_ref_expression(tokens, context)?;

    Some(CaseBranch {
        column_type: Some(ValueType::from_sqlite_type(&schema_column.declared_type)),
        nullable: schema_column.nullable || context.nullable_tables.contains(&table),
    })
}

fn infer_column_ref_expression<'a>(
    tokens: &[Token],
    context: &ExpressionContext<'a>,
) -> Option<(String, &'a SchemaColumn)> {
    let column_ref = column_ref_from_expression(tokens)?;
    resolve_column_ref_with_table(context.schema, context.table_refs, &column_ref)
}

fn column_ref_from_expression(tokens: &[Token]) -> Option<ColumnRef> {
    match tokens {
        [Token::Word(_)] | [Token::QuotedId(_)] => column_ref_starting_at(tokens, 0),
        [Token::Word(_), Token::Dot, Token::Word(_)]
        | [Token::Word(_), Token::Dot, Token::QuotedId(_)]
        | [Token::QuotedId(_), Token::Dot, Token::Word(_)]
        | [Token::QuotedId(_), Token::Dot, Token::QuotedId(_)] => column_ref_starting_at(tokens, 0),
        _ => None,
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

    fn analyze_single_query(schema_sql: &str, query_sql: &str) -> Vec<Parameter> {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(schema_sql).unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("queries/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(sql_dir.join("query.sql"), query_sql).unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        project.queries[0].parameters.clone()
    }

    #[test]
    fn extracts_unique_named_parameters_in_encounter_order() {
        let params = parameters(
            "where org_id = @org_id or parent_id = @org_id and x = @x",
            &Schema::default(),
        );
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
        let params = parameters(
            "where user_id = @user_id and created_at >= :since and name like $pattern or id = :user_id",
            &Schema::default(),
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
    fn infers_dollar_named_parameter_types_from_where_columns() {
        let params = analyze_single_query(
            "create table users (id integer primary key, name text not null);",
            "select id, name from users where id = $id and name = $name",
        );

        assert_eq!(
            params,
            [
                Parameter {
                    name: "id".to_string(),
                    sql_names: vec!["$id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Parameter {
                    name: "name".to_string(),
                    sql_names: vec!["$name".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn ignores_named_parameter_tokens_inside_strings_identifiers_and_comments() {
        let params = parameters(
            r#"
            select '@not_param', ":also_not_param", id
            from users
            where name = @name -- @comment_param
              and bio = 'literal :still_not_param'
              and note = /* $block_param */ $note
            "#,
            &Schema::default(),
        );
        let names = params
            .into_iter()
            .map(|param| param.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["name", "note"]);
    }

    #[test]
    fn extracts_anonymous_parameters_in_encounter_order() {
        let params = parameters("where name = ? and age > ?", &Schema::default());
        let names = params
            .into_iter()
            .map(|param| (param.name, param.sql_names))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                ("param".to_string(), vec![]),
                ("param_2".to_string(), vec![])
            ]
        );
    }

    #[test]
    fn deduplicates_anonymous_and_numbered_parameters_that_share_a_sqlite_slot() {
        let params = parameters("where id = ? and parent_id = ?1", &Schema::default());
        let names = params
            .into_iter()
            .map(|param| (param.name, param.sql_names))
            .collect::<Vec<_>>();

        assert_eq!(names, [("param".to_string(), vec!["?1".to_string()])]);
    }

    #[test]
    fn suffixes_numbered_parameter_names_that_collide_with_named_parameters() {
        let params = parameters("where name = @param and id = ?1", &Schema::default());
        let names = params
            .into_iter()
            .map(|param| (param.name, param.sql_names))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                ("param".to_string(), vec!["@param".to_string()]),
                ("param_2".to_string(), vec!["?1".to_string()]),
            ]
        );
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
            sql_dir: None,
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
    fn infers_anonymous_parameter_types_from_where_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null,
                age integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_users.sql"),
            "select id from users where name = ? and age > ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                    param.sql_names.clone(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::String, false, vec![]),
                ("param_2", ValueType::I64, false, vec![]),
            ]
        );
    }

    #[test]
    fn infers_parameter_type_from_indexed_non_first_column() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                username text not null,
                email text not null,
                age integer not null
            );
            create index users_email_idx on users(email);
            insert into users (id, username, email, age)
            values (1, 'alice', 'alice@example.com', 30);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_users.sql"),
            "select id, username, email, age from users where email = ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_numbered_parameter_types_by_sqlite_index() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                parent_id integer,
                name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_users.sql"),
            "select id from users where id = ?1 or parent_id = ?1 or name = ?2",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                    param.sql_names.clone(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, false, vec!["?1".to_string()]),
                ("param_2", ValueType::String, false, vec!["?2".to_string()]),
            ]
        );
    }

    #[test]
    fn infers_mixed_numbered_and_anonymous_parameters_by_sqlite_slot() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_users.sql"),
            "select id from users where id = ?1 and name = ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                    param.sql_names.clone(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, false, vec!["?1".to_string()]),
                ("param_2", ValueType::String, false, vec![]),
            ]
        );
    }

    #[test]
    fn infers_update_set_and_where_parameter_types_without_spaces() {
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
            sql_dir.join("update_user.sql"),
            "update users set name=? where id=?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::String, false),
                ("param_2", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_update_parameters_inside_coalesce_assignments() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table participants (
                id integer primary key,
                gender text,
                birthdate text,
                updated_at integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("participants/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("update_participant.sql"),
            "
            update participants
            set gender = coalesce(@gender, gender),
                birthdate = coalesce(@birthdate, birthdate),
                updated_at = @updated_at
            where id = @id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("gender", ValueType::String, true),
                ("birthdate", ValueType::String, true),
                ("updated_at", ValueType::I64, false),
                ("id", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn update_with_nested_function_assignment_keeps_where_parameter_inference() {
        let params = analyze_single_query(
            "create table t (id integer primary key, name text not null);",
            "update t set name = lower(trim(name)) where id = ?",
        );

        assert_eq!(
            params,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn update_string_literals_do_not_split_where_clause() {
        let params = analyze_single_query(
            "create table t (id integer primary key, name text not null);",
            "update t set name = 'hello WHERE world' where id = ?",
        );

        assert_eq!(
            params,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn update_with_eq_subquery_infers_set_and_nested_where_parameters() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table waitlist_registrations (
                id integer primary key,
                item_id integer not null,
                account_id integer not null,
                org_id integer not null,
                claimed_at integer,
                updated_at integer not null,
                approved_at integer,
                cancelled_at integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("waitlist/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("claim_registration.sql"),
            "
            update waitlist_registrations
            set claimed_at = @claimed_at, updated_at = @updated_at
            where id = (
                select wr.id
                from waitlist_registrations wr
                where wr.item_id = @item_id
                  and wr.account_id = @account_id
                  and wr.org_id = @org_id
                  and wr.approved_at is not null
                  and wr.claimed_at is null
                  and wr.cancelled_at is null
                order by wr.approved_at asc
                limit 1
            )
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("claimed_at", ValueType::I64, true),
                ("updated_at", ValueType::I64, false),
                ("item_id", ValueType::I64, false),
                ("account_id", ValueType::I64, false),
                ("org_id", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn read_parameter_against_nullable_column_is_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table tasks (
                id integer primary key,
                account_id integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("tasks/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_tasks.sql"),
            "select id from tasks where account_id = @account_id",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "account_id".to_string(),
                sql_names: vec!["@account_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn read_parameter_with_is_operator_uses_column_type_and_is_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table standings (
                id integer primary key,
                season integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("standings/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_standings.sql"),
            "select id from standings where season is @season",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "season".to_string(),
                sql_names: vec!["@season".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn null_guard_read_parameters_are_non_nullable() {
        for (file_name, sql, expected_name) in [
            (
                "prefix_null_guard.sql",
                "select id from tasks where (@account_id is null or account_id = @account_id)",
                "account_id",
            ),
            (
                "suffix_null_guard.sql",
                "select id from tasks where account_id = @account_id or @account_id is null",
                "account_id",
            ),
            (
                "not_null_guard.sql",
                "select id from tasks where @account_id is not null and account_id = @account_id",
                "account_id",
            ),
            (
                "range_null_guard.sql",
                "select id from tasks where @from_date is null or created_at >= @from_date",
                "from_date",
            ),
        ] {
            let dir = tempdir().unwrap();
            let database = dir.path().join("app.sqlite3");
            let conn = Connection::open(&database).unwrap();
            conn.execute_batch(
                "
                create table tasks (
                    id integer primary key,
                    account_id integer not null,
                    created_at integer not null
                );
                ",
            )
            .unwrap();
            drop(conn);

            let source_root = dir.path().join("src");
            let sql_dir = source_root.join("tasks/sql");
            fs::create_dir_all(&sql_dir).unwrap();
            fs::write(sql_dir.join(file_name), sql).unwrap();

            let project = analyze_project(&Config {
                database,
                source_root,
                sql_dir: None,
                output: dir.path().join("generated"),
                target: Target::Rust,
                check: false,
            })
            .unwrap();

            assert_eq!(
                project.queries[0].parameters,
                [Parameter {
                    name: expected_name.to_string(),
                    sql_names: vec![format!("@{expected_name}")],
                    column_type: ValueType::I64,
                    nullable: false,
                }],
                "{file_name}"
            );
        }
    }

    #[test]
    fn delete_where_read_parameter_against_nullable_column_is_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table tasks (
                id integer primary key,
                account_id integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("tasks/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("delete_tasks.sql"),
            "delete from tasks where account_id = @account_id",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "account_id".to_string(),
                sql_names: vec!["@account_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn update_write_nullable_column_but_read_parameter_is_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table tasks (
                id integer primary key,
                account_id integer,
                deleted_at integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("tasks/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("soft_delete_tasks.sql"),
            "
            update tasks
            set deleted_at = @deleted_at
            where account_id = @account_id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "deleted_at".to_string(),
                    sql_names: vec!["@deleted_at".to_string()],
                    column_type: ValueType::I64,
                    nullable: true,
                },
                Parameter {
                    name: "account_id".to_string(),
                    sql_names: vec!["@account_id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn read_parameter_inside_select_list_subquery_resolves_column_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table feature_requests (
                id integer primary key
            );
            create table feature_request_votes (
                id integer primary key,
                feature_request_id integer not null,
                org_id integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("feature_requests/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_feature_requests.sql"),
            "
            select (
                select count(*)
                from feature_request_votes frv
                where frv.org_id = @org_id
            ) as vote_count
            from feature_requests
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "org_id".to_string(),
                sql_names: vec!["@org_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn correlated_select_list_subquery_parameters_resolve_column_types() {
        let parameters = analyze_single_query(
            "
            create table feature_requests (
                id integer primary key
            );
            create table feature_request_votes (
                id integer primary key,
                feature_request_id integer not null,
                org_id integer not null
            );
            ",
            "
            select
                fr.id,
                cast((
                    select count(*)
                    from feature_request_votes frv
                    where frv.feature_request_id = fr.id
                      and frv.org_id = @org_id
                ) as integer) as has_voted
            from feature_requests fr
            where fr.id = @id
            ",
        );

        assert_eq!(
            parameters,
            [
                Parameter {
                    name: "org_id".to_string(),
                    sql_names: vec!["@org_id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Parameter {
                    name: "id".to_string(),
                    sql_names: vec!["@id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn derived_table_subquery_parameters_resolve_column_types() {
        let parameters = analyze_single_query(
            "
            create table participants (
                id integer primary key,
                org_id integer not null
            );
            ",
            "
            select count(*) from (
                select id from participants p where p.org_id = @org_id
            )
            ",
        );

        assert_eq!(
            parameters,
            [Parameter {
                name: "org_id".to_string(),
                sql_names: vec!["@org_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn derived_table_exists_parameters_resolve_names_and_types() {
        let parameters = analyze_single_query(
            "
            create table participants (
                id integer primary key,
                name text not null
            );
            create table participant_orgs (
                participant_id integer not null,
                org_id integer not null
            );
            ",
            "
            select count(*) from (
                select distinct p.id
                from participants p
                where exists (
                    select 1
                    from participant_orgs po
                    where po.participant_id = p.id
                      and po.org_id = @org_id
                )
                and cast(@search as text) = ''
            )
            ",
        );

        assert_eq!(
            parameters,
            [
                Parameter {
                    name: "org_id".to_string(),
                    sql_names: vec!["@org_id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Parameter {
                    name: "search".to_string(),
                    sql_names: vec!["@search".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn count_wrapper_preserves_real_world_parameter_names() {
        let parameters = analyze_single_query(
            "
            create table participants (
                id integer primary key,
                first_name text not null,
                last_name text not null,
                email text not null
            );
            create table line_items (
                participant_id integer not null,
                item_id integer
            );
            create table items (
                id integer primary key,
                org_id integer not null,
                season integer
            );
            create table waiver_responses (
                participant_id integer,
                org_id integer not null
            );
            ",
            "
            select cast(count(*) as integer) as count from (
                select distinct p.id
                from participants p
                where (
                    exists (
                        select 1 from line_items li2
                        join items pr2 on li2.item_id = pr2.id
                        where li2.participant_id = p.id and pr2.org_id = @org_id
                    )
                    or exists (
                        select 1 from waiver_responses wa
                        where wa.participant_id = p.id and wa.org_id = @org_id
                    )
                )
                and (cast(@search as text) = ''
                    or p.first_name like cast(@search as text) || '%'
                    or p.last_name like cast(@search as text) || '%'
                    or p.email like cast(@search as text) || '%')
                and (cast(@season as integer) = 0 or exists (
                    select 1 from line_items li3
                    join items pr3 on li3.item_id = pr3.id
                    where li3.participant_id = p.id
                      and pr3.org_id = @org_id
                      and pr3.season = cast(@season as integer)
                ))
            )
            ",
        );

        assert_eq!(
            parameters,
            [
                Parameter {
                    name: "org_id".to_string(),
                    sql_names: vec!["@org_id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Parameter {
                    name: "search".to_string(),
                    sql_names: vec!["@search".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "season".to_string(),
                    sql_names: vec!["@season".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn cte_result_columns_keep_underlying_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                org_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("recent_orders.sql"),
            "
            with recent as (
                select id from orders where org_id = @org_id
            )
            select id from recent
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "id".to_string(),
                field_name: "id".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "org_id".to_string(),
                sql_names: vec!["@org_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn recursive_cte_result_and_parameter_use_seed_column_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        Connection::open(&database).unwrap().close().unwrap();

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("reports/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("count_to_limit.sql"),
            "
            with recursive counter(n) as (
                select 1
                union all
                select n + 1 from counter where n < @limit
            )
            select n from counter
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "n".to_string(),
                field_name: "n".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "limit".to_string(),
                sql_names: vec!["@limit".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn multiple_ctes_can_infer_from_previous_cte_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                org_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_order_ids.sql"),
            "
            with filtered(id) as (
                select id from orders where org_id = @org_id
            ),
            ids(id) as (
                select id from filtered
            )
            select id from ids
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "id".to_string(),
                field_name: "id".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "org_id".to_string(),
                sql_names: vec!["@org_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_update_parameters_inside_in_subquery() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table line_item_question_values (
                id integer primary key,
                line_item_id integer not null,
                question_key text not null,
                question_name text
            );
            create table line_items (
                id integer primary key,
                order_id integer not null
            );
            create table orders (
                id integer primary key,
                org_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("questions/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("update_question_names.sql"),
            "
            update line_item_question_values
            set question_name = @question_name
            where question_key = @question_key
              and line_item_id in (
                select li.id
                from line_items li
                join orders o on o.id = li.order_id
                where o.org_id = @org_id
              )
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("question_name", ValueType::String, true),
                ("question_key", ValueType::String, false),
                ("org_id", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_named_parameter_type_from_qualified_alias_column() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                email text not null
            );
            create table orders (
                id text primary key,
                user_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_order.sql"),
            "select u.email from users u join orders o on o.user_id = u.id where o.id = @order_id",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "order_id".to_string(),
                sql_names: vec!["@order_id".to_string()],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn sanitizes_reserved_words_in_named_parameters() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            r#"
            create table things (
                id integer primary key,
                "type" text not null
            );
            "#,
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_by_type.sql"),
            r#"select id from things where "type" = @type"#,
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "type_".to_string(),
                sql_names: vec!["@type".to_string()],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn suffixes_colliding_generated_parameter_names() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            r#"
            create table things (
                id integer primary key,
                "type" text not null,
                type_ text not null
            );
            "#,
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_by_types.sql"),
            r#"select id from things where "type" = @type and type_ = @type_"#,
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "type_".to_string(),
                    sql_names: vec!["@type".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "type__2".to_string(),
                    sql_names: vec!["@type_".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_insert_parameter_types_and_nullability_from_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key autoincrement,
                username text not null,
                bio text,
                created_at integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_user.sql"),
            "insert into users (username, bio, created_at) values (@username, @bio, @created_at)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("username", ValueType::String, false),
                ("bio", ValueType::String, true),
                ("created_at", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_insert_parameters_when_literals_are_interleaved_with_values() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table entries (
                order_id integer not null,
                kind text not null,
                item_id integer,
                description text not null,
                amount integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("entries/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_entry.sql"),
            "
            insert into entries (order_id, kind, item_id, description, amount)
            values (@order_id, 'adjustment', null, @description, @amount)
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("order_id", ValueType::I64, false),
                ("description", ValueType::String, false),
                ("amount", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_insert_select_parameters_from_target_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table item_features (
                id integer primary key,
                item_id integer not null,
                field_key text not null,
                created_at integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("features/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("copy_features.sql"),
            "
            insert into item_features (item_id, field_key, created_at)
            select @item_id, lf.field_key, @created_at
            from item_features lf
            where lf.item_id = @source_item_id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "item_id".to_string(),
                    sql_names: vec!["@item_id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Parameter {
                    name: "created_at".to_string(),
                    sql_names: vec!["@created_at".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Parameter {
                    name: "source_item_id".to_string(),
                    sql_names: vec!["@source_item_id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_insert_values_without_column_list_from_schema_order() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null,
                age integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_user.sql"),
            "insert into users values (?, ?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, true),
                ("param_2", ValueType::String, false),
                ("param_3", ValueType::I64, true),
            ]
        );
    }

    #[test]
    fn insert_values_without_column_list_skips_generated_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table metrics (
                id integer primary key,
                value integer not null,
                doubled integer generated always as (value * 2) virtual
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("metrics/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_metric.sql"),
            "insert into metrics values (?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, true),
                ("param_2", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_insert_values_parameters_across_multiple_rows() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table line_items (
                quantity integer not null,
                label text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("line_items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_line_items.sql"),
            "insert into line_items values (1, ?), (?, 'second')",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::String, false),
                ("param_2", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn insert_values_without_rowid_primary_key_is_not_nullable_on_write() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text
            ) without rowid;
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_user.sql"),
            "insert into users values (?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, false),
                ("param_2", ValueType::String, true),
            ]
        );
    }

    #[test]
    fn insert_values_int_primary_key_is_not_treated_as_rowid_alias_on_write() {
        for (table_name, primary_key_type) in [("int_t", "int"), ("bigint_t", "bigint")] {
            let dir = tempdir().unwrap();
            let database = dir.path().join("app.sqlite3");
            let conn = Connection::open(&database).unwrap();
            conn.execute_batch(&format!(
                "create table {table_name} (id {primary_key_type} primary key, name text not null);"
            ))
            .unwrap();
            drop(conn);

            let source_root = dir.path().join("src");
            let sql_dir = source_root.join("items/sql");
            fs::create_dir_all(&sql_dir).unwrap();
            fs::write(
                sql_dir.join("insert_item.sql"),
                format!("insert into {table_name} (id, name) values (?, ?)"),
            )
            .unwrap();

            let project = analyze_project(&Config {
                database,
                source_root,
                sql_dir: None,
                output: dir.path().join("generated"),
                target: Target::Rust,
                check: false,
            })
            .unwrap();

            assert_eq!(
                project.queries[0].parameters,
                [
                    Parameter {
                        name: "param".to_string(),
                        sql_names: vec![],
                        column_type: ValueType::I64,
                        nullable: true,
                    },
                    Parameter {
                        name: "param_2".to_string(),
                        sql_names: vec![],
                        column_type: ValueType::String,
                        nullable: false,
                    },
                ],
                "{primary_key_type} primary key should not be treated as an integer rowid alias"
            );
        }
    }

    #[test]
    fn insert_values_composite_primary_key_columns_are_nullable_on_write() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table memberships (
                user_id integer,
                org_id integer,
                primary key (user_id, org_id)
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("memberships/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_membership.sql"),
            "insert into memberships values (?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, true),
                ("param_2", ValueType::I64, true),
            ]
        );
    }

    #[test]
    fn rejects_insert_values_count_mismatch_with_row_number() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table events (
                id integer,
                label text
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("events/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_events.sql"),
            "insert into events values (?, ?), (?)",
        )
        .unwrap();

        let result = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        });

        assert!(matches!(
            result,
            Err(Error::InsertValuesCountMismatch {
                expected: 2,
                got: 1,
                row: 2,
                ..
            })
        ));
    }

    #[test]
    fn rejects_explicit_insert_values_count_mismatch() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table events (
                id integer,
                label text,
                created_at integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("events/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_events.sql"),
            "insert into events (id, label) values (?, ?, ?)",
        )
        .unwrap();

        let result = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        });

        assert!(matches!(
            result,
            Err(Error::InsertValuesCountMismatch {
                expected: 2,
                got: 3,
                row: 1,
                ..
            })
        ));
    }

    #[test]
    fn missing_insert_target_table_falls_through_to_prepare_error() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        Connection::open(&database).unwrap();

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("events/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_events.sql"),
            "insert into missing values (?)",
        )
        .unwrap();

        let result = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        });

        assert!(matches!(result, Err(Error::PrepareSql { .. })));
    }

    #[test]
    fn infers_insert_or_replace_parameter_types_and_primary_key_nullability() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("replace_user.sql"),
            "insert or replace into users (id, name) values (?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, true),
                ("param_2", ValueType::String, false),
            ]
        );
    }

    #[test]
    fn infers_insert_conflict_action_variants_like_insert() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_ignore.sql"),
            "insert or ignore into users (id, name) values (?, ?)",
        )
        .unwrap();
        fs::write(
            sql_dir.join("insert_fail.sql"),
            "insert or fail into users (id, name) values (?, ?)",
        )
        .unwrap();
        fs::write(
            sql_dir.join("insert_rollback.sql"),
            "insert or rollback into users (id, name) values (?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        for query_name in ["insert_fail", "insert_ignore", "insert_rollback"] {
            let query = project
                .queries
                .iter()
                .find(|query| query.name == query_name)
                .unwrap();
            assert!(matches!(query.return_type, ReturnType::Execute));
            assert_eq!(
                query.parameters,
                [
                    Parameter {
                        name: "param".to_string(),
                        sql_names: vec![],
                        column_type: ValueType::I64,
                        nullable: true,
                    },
                    Parameter {
                        name: "param_2".to_string(),
                        sql_names: vec![],
                        column_type: ValueType::String,
                        nullable: false,
                    },
                ],
                "{query_name}"
            );
        }
    }

    #[test]
    fn analyzes_insert_default_values_as_execute_without_parameters() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table events (
                id integer primary key,
                name text not null default 'untitled'
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("events/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("create_event.sql"),
            "insert into events default values",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert!(matches!(
            project.queries[0].return_type,
            ReturnType::Execute
        ));
        assert!(project.queries[0].parameters.is_empty());
        assert!(project.queries[0].columns.is_empty());
    }

    #[test]
    fn infers_replace_into_parameter_types_like_insert() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("replace_user.sql"),
            "replace into users (id, name) values (?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, true),
                ("param_2", ValueType::String, false),
            ]
        );
    }

    #[test]
    fn infers_upsert_do_update_parameter_types_from_insert_table() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                val text not null,
                version integer not null default 0
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("upsert_thing.sql"),
            "
            insert into t (id, val, version)
            values (?, ?, ?)
            on conflict(id) do update
            set val = ?, version = ?
            where version < ?
            returning id, val, version
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::I64, true),
                ("param_2", ValueType::String, false),
                ("param_3", ValueType::I64, false),
                ("param_4", ValueType::String, false),
                ("param_5", ValueType::I64, false),
                ("param_6", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_upsert_do_nothing_returning_from_insert_table() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                val text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_thing.sql"),
            "
            insert into t (id, val)
            values (?, ?)
            on conflict(id) do nothing
            returning id, val
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert!(matches!(
            project.queries[0].return_type,
            ReturnType::Rows { row_type: None }
        ));
        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "param".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::I64,
                    nullable: true,
                },
                Parameter {
                    name: "param_2".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "val".to_string(),
                    field_name: "val".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_upsert_do_nothing_without_returning_as_execute() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                val text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_thing.sql"),
            "
            insert into t (id, val)
            values (?, ?)
            on conflict(id) do nothing
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert!(matches!(
            project.queries[0].return_type,
            ReturnType::Execute
        ));
        assert!(project.queries[0].columns.is_empty());
        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "param".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::I64,
                    nullable: true,
                },
                Parameter {
                    name: "param_2".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn deduplicates_named_upsert_do_update_parameters() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                val text not null,
                counter integer not null default 1
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("upsert_thing.sql"),
            "
            insert into t (id, val, counter)
            values (@id, @val, @counter)
            on conflict(id) do update
            set val = @val, counter = @counter
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "id".to_string(),
                    sql_names: vec!["@id".to_string()],
                    column_type: ValueType::I64,
                    nullable: true,
                },
                Parameter {
                    name: "val".to_string(),
                    sql_names: vec!["@val".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "counter".to_string(),
                    sql_names: vec!["@counter".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn analyzes_upsert_insert_select_returning_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                val text not null
            );
            create table src (
                id integer primary key,
                val text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("copy_things.sql"),
            "
            insert into t (id, val)
            select id, val from src where true
            on conflict(id) do update set val = excluded.val
            returning id, val
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert!(project.queries[0].parameters.is_empty());
        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "val".to_string(),
                    field_name: "val".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_limit_and_offset_parameters_as_i64() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table users (id integer primary key);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_users.sql"),
            "select id from users limit @limit offset @offset",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("limit", ValueType::I64, false),
                ("offset", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_order_by_query_parameters_and_limit_parameter() {
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
            "select id, name from users where name = ? order by id limit ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "param".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "param_2".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_like_parameter_type_from_column() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_users.sql"),
            "select id, name from users where name like ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_not_like_parameter_type_from_column() {
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
            sql_dir.join("find_users.sql"),
            "select id from users where name not like ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn keeps_like_concat_parameter_as_text() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table accounts (id integer primary key, email text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("accounts/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_accounts.sql"),
            "select id, email from accounts where lower(email) like @prefix || '%'",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "prefix".to_string(),
                sql_names: vec!["@prefix".to_string()],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn keeps_not_in_subquery_from_interfering_with_following_parameter_inference() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            create table deleted (
                user_id integer not null
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
            select id, name
            from users
            where id not in (select user_id from deleted)
              and name = ?
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn subquery_where_parameter_uses_subquery_column_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null
            );
            create table orders (
                id integer primary key,
                user_id integer not null,
                total real not null
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
            select id, name
            from users
            where id in (
                select user_id from orders where total > ?
            )
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::F64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn quoted_identifier_containing_placeholder_does_not_create_parameter() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (\"what?\" integer primary key, val text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_thing.sql"),
            "select \"what?\", val from t where val = ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "what?".to_string(),
                    field_name: "what".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "val".to_string(),
                    field_name: "val".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn quoted_identifier_with_escaped_quotes_resolves_column_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "create table t (\"my\"\"col\" integer primary key, name text not null);",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select \"my\"\"col\", name from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "my\"col".to_string(),
                    field_name: "mycol".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "name".to_string(),
                    field_name: "name".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn quoted_keyword_identifier_in_where_infers_parameter_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, \"AND\" text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_thing.sql"),
            "select id from t where \"AND\" = ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn order_by_collate_nocase_keeps_where_parameter_inference() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                last_name text not null,
                first_name text not null
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
            select id, last_name, first_name
            from users
            where id > @min_id
            order by last_name collate nocase, first_name collate nocase
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(project.queries[0].columns.len(), 3);
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "min_id".to_string(),
                sql_names: vec!["@min_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn extracts_parameters_from_in_literal_list() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                status text not null
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
            "select id from users where status in (?, ?, ?)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("param", ValueType::String, false),
                ("param_2", ValueType::String, false),
                ("param_3", ValueType::String, false),
            ]
        );
    }

    #[test]
    fn string_keywords_do_not_disrupt_where_parameter_inference() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null,
                email text not null,
                status text not null
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
            select id
            from users
            where name != 'foo AND bar'
              and email != 'yes or no'
              and status = ?
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn escaped_quote_does_not_hide_following_placeholder() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, name text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_thing.sql"),
            "select id from t where name != 'it''s' and id = ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_parameters_from_in_list_column_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table fees (
                id integer primary key,
                club_id integer not null,
                active integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("fees/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("count_fees.sql"),
            "select count(*) from fees where club_id in (@club_id, @parent_id, @grandparent_id) and active = @active",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("club_id", ValueType::I64, false),
                ("parent_id", ValueType::I64, false),
                ("grandparent_id", ValueType::I64, false),
                ("active", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_not_between_parameter_types_from_column() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table events (
                id integer primary key,
                created_at integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("events/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_events.sql"),
            "select id from events where created_at not between @start and @end",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("start", ValueType::I64, false),
                ("end", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_having_aggregate_parameter_type_from_aggregate_column() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                region text not null,
                amount integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("region_totals.sql"),
            "
            select region, sum(amount) as total
            from orders
            group by region
            having sum(amount) > @min_amount
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "min_amount".to_string(),
                sql_names: vec!["@min_amount".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_parameter_type_from_cast_target() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table events (id integer primary key);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("events/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_events.sql"),
            "select id from events where cast(@season as integer) = 0",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "season".to_string(),
                sql_names: vec!["@season".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
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
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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
    fn left_join_on_unindexed_column_marks_right_side_nullable() {
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
                user_name text not null,
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
            left join profiles p on p.user_name = u.name
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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

        assert_eq!(facts, [("name", false), ("bio", true)]);
    }

    #[test]
    fn right_join_marks_left_side_result_columns_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                org_id integer not null,
                name text not null
            );
            create table orgs (
                id integer primary key,
                org_name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_org_users.sql"),
            "
            select u.id, o.org_name
            from users u
            right join orgs o on u.org_id = o.id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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

        assert_eq!(facts, [("id", true), ("org_name", false)]);
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
            sql_dir: None,
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
    fn cross_join_keeps_result_columns_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table a (id integer primary key, val_a text not null);
            create table b (id integer primary key, val_b text not null);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select a.id, b.id as b_id from a cross join b",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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

        assert_eq!(facts, [("id", false), ("b_id", false)]);
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
            sql_dir: None,
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
            sql_dir: None,
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
    fn natural_join_result_columns_use_origin_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table a (id integer primary key, val_a text not null);
            create table b (id integer primary key, val_b text not null);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select a.id, a.val_a from a natural join b",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "val_a".to_string(),
                    field_name: "val_a".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn natural_left_join_uses_origin_metadata_and_left_side_nullability() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table a (id integer primary key, val_a text not null);
            create table b (id integer primary key, val_b text not null);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select a.id, a.val_a from a natural left join b",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "val_a".to_string(),
                    field_name: "val_a".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn join_using_result_columns_use_origin_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                org_id integer not null,
                total integer not null
            );
            create table line_items (
                id integer primary key,
                org_id integer not null,
                product text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_orders.sql"),
            "select o.total, l.product from orders o join line_items l using (org_id)",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "total".to_string(),
                    field_name: "total".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "product".to_string(),
                    field_name: "product".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn left_join_strength_reduced_by_where_keeps_result_column_non_nullable() {
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
            select p.bio
            from users u
            left join profiles p on p.user_id = u.id
            where p.bio = 'x'
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "bio".to_string(),
                field_name: "bio".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
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
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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
    fn avg_min_and_max_return_aggregate_result_types() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table scores (
                score integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("scores/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("summarize_scores.sql"),
            "
            select avg(score) as avg_score, min(score) as min_score, max(score) as max_score
            from scores
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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
                ("avg_score", ValueType::F64, true),
                ("min_score", ValueType::I64, true),
                ("max_score", ValueType::I64, true),
            ]
        );
    }

    #[test]
    fn literal_result_expressions_infer_result_types() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("reports/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("literal_values.sql"),
            "
            select 42 as answer,
                   -5 as debt,
                   3.14 as ratio,
                   -3.14 as negative_ratio,
                   'hello' as greeting
            from t
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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
                ("answer", ValueType::I64, false),
                ("debt", ValueType::I64, false),
                ("ratio", ValueType::F64, false),
                ("negative_ratio", ValueType::F64, false),
                ("greeting", ValueType::String, false),
            ]
        );
    }

    #[test]
    fn count_distinct_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                customer_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("unique_customers.sql"),
            "select count(distinct customer_id) as unique_customers from orders",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "unique_customers".to_string(),
                field_name: "unique_customers".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn exists_subquery_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (id integer primary key);
            create table events (id integer primary key, user_id integer not null);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("user_flags.sql"),
            "
            select exists(select 1 from events where events.user_id = users.id) as has_events
            from users
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "has_events".to_string(),
                field_name: "has_events".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn cast_subquery_column_as_integer_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, val integer not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("cast_value.sql"),
            "
            select cast(sub.val as integer) as v
            from (select val from t) sub
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "v".to_string(),
                field_name: "v".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn string_literals_do_not_split_select_expressions() {
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
            sql_dir.join("display_user.sql"),
            "select coalesce(name, 'unknown, unnamed') as display_name from users where id = ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "display_name".to_string(),
                field_name: "display_name".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn string_literals_do_not_change_parenthesis_depth() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, name text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("display_thing.sql"),
            "select coalesce(name, 'default)value') as display from t where id = ?",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "display".to_string(),
                field_name: "display".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn select_distinct_with_where_param_preserves_metadata_and_param_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table tickets (
                id integer primary key,
                status text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("tickets/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_statuses.sql"),
            "select distinct status from tickets where id > ? order by status",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "status".to_string(),
                field_name: "status".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn select_distinct_multiple_columns_and_order_by_preserve_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table tickets (
                id integer primary key,
                status text not null,
                priority integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("tickets/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_ticket_states.sql"),
            "
            select distinct status, priority
            from tickets
            order by status, priority
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "status".to_string(),
                    field_name: "status".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
                Column {
                    name: "priority".to_string(),
                    field_name: "priority".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn group_by_with_multiple_aggregates_infers_result_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table measurements (
                id integer primary key,
                grp text not null,
                val integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("measurements/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("summarize.sql"),
            "
            select
                grp,
                count(*) as cnt,
                avg(val) as avg_val,
                max(val) as max_val
            from measurements
            group by grp
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "grp".to_string(),
                    field_name: "grp".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
                Column {
                    name: "cnt".to_string(),
                    field_name: "cnt".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "avg_val".to_string(),
                    field_name: "avg_val".to_string(),
                    column_type: ValueType::F64,
                    nullable: true,
                },
                Column {
                    name: "max_val".to_string(),
                    field_name: "max_val".to_string(),
                    column_type: ValueType::I64,
                    nullable: true,
                },
            ]
        );
    }

    #[test]
    fn compound_query_as_subquery_infers_outer_where_param() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table active_users (
                id integer primary key,
                name text not null
            );
            create table archived_users (
                id integer primary key,
                name text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("find_all.sql"),
            "
            select id, name
            from (
                select id, name from active_users
                union
                select id, name from archived_users
            )
            where name = ?
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "name".to_string(),
                    field_name: "name".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn intersect_and_except_preserve_result_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t1 (id integer primary key);
            create table t2 (id integer primary key);
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("common_things.sql"),
            "select id from t1 intersect select id from t2",
        )
        .unwrap();
        fs::write(
            sql_dir.join("remaining_things.sql"),
            "select id from t1 except select id from t2",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        for query_name in ["common_things", "remaining_things"] {
            let query = project
                .queries
                .iter()
                .find(|query| query.name == query_name)
                .unwrap();
            assert_eq!(
                query.columns,
                [Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                }],
                "{query_name}"
            );
        }
    }

    #[test]
    fn aggregate_filter_where_preserves_aggregate_result_types_and_params() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                status text not null,
                amount real not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("filtered_totals.sql"),
            "
            select
                count(*) filter(where status = 'active') as active_count,
                sum(amount) filter(where status = @status) as total
            from orders
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "active_count".to_string(),
                    field_name: "active_count".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "total".to_string(),
                    field_name: "total".to_string(),
                    column_type: ValueType::F64,
                    nullable: true,
                },
            ]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "status".to_string(),
                sql_names: vec!["@status".to_string()],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn cast_count_as_integer_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("count_things.sql"),
            "select cast(count(*) as integer) as count from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "count".to_string(),
                field_name: "count".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn cast_coalesce_sum_as_integer_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                amount_cents integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("sum_things.sql"),
            "select cast(coalesce(sum(amount_cents), 0) as integer) as total from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "total".to_string(),
                field_name: "total".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn nested_cast_returns_outer_cast_type_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                val real not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("convert_things.sql"),
            "select cast(cast(val as integer) as text) as converted from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "converted".to_string(),
                field_name: "converted".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn returning_cast_alias_uses_cast_result_type() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, val text not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_thing.sql"),
            "insert into t (val) values (?) returning cast(id as text) as id_text",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "id_text".to_string(),
                field_name: "id_text".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn returning_direct_column_alias_preserves_case_and_origin_type() {
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
            sql_dir.join("insert_user.sql"),
            "insert into users (name) values (?) returning id as userId, name as userName",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "userId".to_string(),
                    field_name: "userid".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "userName".to_string(),
                    field_name: "username".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn insert_returning_uses_insert_params_and_returned_column_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key autoincrement,
                username text not null,
                created_at timestamp not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_user.sql"),
            "insert into users (username, created_at) values (?, ?) returning id, created_at",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "created_at".to_string(),
                    field_name: "created_at".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "param".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "param_2".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn update_returning_uses_set_where_params_and_returned_column_metadata() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                name text not null,
                updated_at timestamp not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("update_user.sql"),
            "update users set name = ?, updated_at = ? where id = ? returning id, updated_at",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "updated_at".to_string(),
                    field_name: "updated_at".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "param".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "param_2".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "param_3".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_update_parameters_inside_column_arithmetic() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table credits (
                id integer primary key,
                balance integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("credits/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("add_credit.sql"),
            "
            update credits
            set balance = balance + @delta
            where id = @id and balance + @min_delta >= 0
            returning id, balance
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        let facts = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            facts,
            [
                ("delta", ValueType::I64, false),
                ("id", ValueType::I64, false),
                ("min_delta", ValueType::I64, false),
            ]
        );
    }

    #[test]
    fn infers_select_parameter_inside_multiplication_expression() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table line_items (
                id integer primary key,
                quantity integer not null,
                unit_price integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("line_items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("expensive_items.sql"),
            "select id from line_items where quantity * unit_price >= quantity * @threshold",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "threshold".to_string(),
                sql_names: vec!["@threshold".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_parameter_inside_cte_body() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                org_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("count_filtered.sql"),
            "
            with filtered as (
                select id from orders where org_id = @org_id
            )
            select count(*) from filtered
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "org_id".to_string(),
                sql_names: vec!["@org_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_parameter_in_second_union_arm() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key,
                org_id integer not null
            );
            create table archived_users (
                id integer primary key,
                org_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_all_users.sql"),
            "
            select id from users
            union all
            select id from archived_users where org_id = @org_id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "org_id".to_string(),
                sql_names: vec!["@org_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_parameter_in_join_on_clause() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table participants (
                id integer primary key
            );
            create table line_items (
                id integer primary key,
                participant_id integer not null,
                org_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("participants/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_participants.sql"),
            "
            select p.id
            from participants p
            join line_items li
              on li.participant_id = p.id
             and li.org_id = @org_id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "org_id".to_string(),
                sql_names: vec!["@org_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_parameter_in_update_from_where_clause() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table participants (
                id integer primary key,
                name text not null
            );
            create table line_items (
                id integer primary key,
                participant_id integer not null,
                org_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("participants/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("rename_participants.sql"),
            "
            update participants
            set name = @name
            from line_items li
            where li.participant_id = participants.id
              and li.org_id = @org_id
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "name".to_string(),
                    sql_names: vec!["@name".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "org_id".to_string(),
                    sql_names: vec!["@org_id".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_parameter_from_inner_alias_shadowing_outer_alias() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key
            );
            create table audit_logs (
                id integer primary key,
                actor_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_actor_users.sql"),
            "
            select *
            from users u
            where exists (
                select 1 from audit_logs u where u.actor_id = @actor_id
            )
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "actor_id".to_string(),
                sql_names: vec!["@actor_id".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn infers_parameter_inside_not_exists_subquery() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table participants (
                id integer primary key,
                name text not null
            );
            create table line_items (
                participant_id integer not null,
                status text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("participants/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("without_status.sql"),
            "
            select p.id, p.name
            from participants p
            where not exists (
                select 1
                from line_items li
                where li.participant_id = p.id
                  and li.status = @status
            )
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "status".to_string(),
                sql_names: vec!["@status".to_string()],
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }

    #[test]
    fn deduplicates_named_parameters_reused_in_order_by_case() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table payments (
                id integer primary key,
                created_at integer not null,
                deposited_on integer
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("payments/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_payments.sql"),
            "
            select id, created_at, deposited_on
            from payments
            where (@by_date = 'created_at' and created_at >= @from_ts)
               or (@by_date = 'deposited_on' and deposited_on is not null and deposited_on >= @from_ts)
            order by case when @by_date = 'deposited_on' then deposited_on end desc, created_at desc
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "by_date".to_string(),
                    sql_names: vec!["@by_date".to_string()],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "from_ts".to_string(),
                    sql_names: vec!["@from_ts".to_string()],
                    column_type: ValueType::I64,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn infers_case_result_parameter_from_compared_column() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table orders (
                id integer primary key,
                status_rank integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("orders/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("ranked_orders.sql"),
            "
            select id
            from orders
            where case
                when @rank = 0 then status_rank
                else @rank
            end = status_rank
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "rank".to_string(),
                sql_names: vec!["@rank".to_string()],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn delete_returning_uses_where_params_and_returned_column_metadata() {
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
            sql_dir.join("delete_user.sql"),
            "delete from users where id = ? returning id, name",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "name".to_string(),
                    field_name: "name".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
        assert_eq!(
            project.queries[0].parameters,
            [Parameter {
                name: "param".to_string(),
                sql_names: vec![],
                column_type: ValueType::I64,
                nullable: false,
            }]
        );
    }

    #[test]
    fn returning_star_expands_table_columns() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table users (
                id integer primary key autoincrement,
                name text not null,
                email text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("users/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("insert_user.sql"),
            "insert into users (name, email) values (?, ?) returning *",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [
                Column {
                    name: "id".to_string(),
                    field_name: "id".to_string(),
                    column_type: ValueType::I64,
                    nullable: false,
                },
                Column {
                    name: "name".to_string(),
                    field_name: "name".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
                Column {
                    name: "email".to_string(),
                    field_name: "email".to_string(),
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
        assert_eq!(
            project.queries[0].parameters,
            [
                Parameter {
                    name: "param".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
                Parameter {
                    name: "param_2".to_string(),
                    sql_names: vec![],
                    column_type: ValueType::String,
                    nullable: false,
                },
            ]
        );
    }

    #[test]
    fn coalesce_max_plus_literal_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table items (
                id integer primary key,
                org_id integer not null,
                item_type text not null,
                season integer not null,
                position integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("items/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("next_position.sql"),
            "
            select coalesce(max(position), 0) + 1 as next_position
            from items
            where org_id = @org_id
              and item_type = @item_type
              and season is @season
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "next_position".to_string(),
                field_name: "next_position".to_string(),
                column_type: ValueType::I64,
                nullable: false,
            }]
        );

        let params = project.queries[0]
            .parameters
            .iter()
            .map(|param| {
                (
                    param.name.as_str(),
                    param.column_type.clone(),
                    param.nullable,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            params,
            [
                ("org_id", ValueType::I64, false),
                ("item_type", ValueType::String, false),
                ("season", ValueType::I64, false),
            ]
        );
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
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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
    fn case_with_exists_condition_returns_i64_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table seasons (
                id integer primary key,
                name text not null
            );
            create table events (
                id integer primary key,
                season_id integer not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("seasons/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_seasons.sql"),
            "
            select
              case
                when exists(select 1 from events where events.season_id = seasons.id)
                then 1
                else 0
              end as registered
            from seasons
            ",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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
            sql_dir: None,
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
    fn case_with_string_literals_containing_end_returns_string_non_nullable() {
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
            "select case when active then 'THE END' else 'no END here' end as label from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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
            sql_dir: None,
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

    #[test]
    fn nested_case_falls_back_to_nullable_string() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                a boolean not null,
                b boolean not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case when a then case when b then 1 else 0 end else 2 end as val from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "val".to_string(),
                field_name: "val".to_string(),
                column_type: ValueType::String,
                nullable: true,
            }]
        );
    }

    #[test]
    fn simple_case_with_string_literals_returns_string_non_nullable() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("create table t (id integer primary key, status integer not null);")
            .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case status when 1 then 'active' when 2 then 'inactive' else 'unknown' end as label from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
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
    fn case_with_column_branches_uses_schema_type_and_nullability() {
        let dir = tempdir().unwrap();
        let database = dir.path().join("app.sqlite3");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch(
            "
            create table t (
                id integer primary key,
                a text not null,
                b text not null
            );
            ",
        )
        .unwrap();
        drop(conn);

        let source_root = dir.path().join("src");
        let sql_dir = source_root.join("things/sql");
        fs::create_dir_all(&sql_dir).unwrap();
        fs::write(
            sql_dir.join("list_things.sql"),
            "select case when id > 0 then a else b end as val from t",
        )
        .unwrap();

        let project = analyze_project(&Config {
            database,
            source_root,
            sql_dir: None,
            output: dir.path().join("generated"),
            target: Target::Rust,
            check: false,
        })
        .unwrap();

        assert_eq!(
            project.queries[0].columns,
            [Column {
                name: "val".to_string(),
                field_name: "val".to_string(),
                column_type: ValueType::String,
                nullable: false,
            }]
        );
    }
}
