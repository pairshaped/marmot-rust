use std::collections::BTreeMap;

use crate::sqlite::tokenize::Token;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBinding {
    pub table: TableRef,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromItem {
    pub binding: TableBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasError {
    Collision(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedColumn<T> {
    pub table: String,
    pub column: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnResolution<T> {
    Resolved(ResolvedColumn<T>),
    AmbiguousColumn,
    UnknownBareColumn,
    UnknownColumnInKnownTable,
    UnknownQualifiedAlias,
    UnknownTableRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStmt {
    pub ctes: Vec<CteDef>,
    pub body: SelectBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CteDef {
    pub name: String,
    pub columns: Vec<String>,
    pub body: Vec<Token>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectBody {
    pub is_distinct: bool,
    pub select_list: Vec<Token>,
    pub from: Vec<FromItem>,
    pub where_clause: Option<Vec<Token>>,
    pub group_by: Option<Vec<Token>>,
    pub having: Option<Vec<Token>>,
    pub order_by: Option<Vec<Token>>,
    pub limit: Option<Vec<Token>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStmt {
    pub ctes: Vec<CteDef>,
    pub conflict_action: InsertConflictAction,
    pub target: TableBinding,
    pub column_list: Option<Vec<String>>,
    pub source: InsertSource,
    pub upsert: Option<Vec<Token>>,
    pub returning: Option<Vec<Token>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertSource {
    Values {
        raw: Vec<Token>,
        rows: Vec<Vec<Vec<Token>>>,
    },
    Select(SelectStmt),
    DefaultValues,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStmt {
    pub ctes: Vec<CteDef>,
    pub target: TableBinding,
    pub set: Vec<Token>,
    pub from: Vec<FromItem>,
    pub where_clause: Option<Vec<Token>>,
    pub returning: Option<Vec<Token>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteStmt {
    pub ctes: Vec<CteDef>,
    pub target: TableBinding,
    pub where_clause: Option<Vec<Token>>,
    pub returning: Option<Vec<Token>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertConflictAction {
    Abort,
    Replace,
    Ignore,
    Fail,
    Rollback,
}

pub fn parse_statement(tokens: &[Token]) -> Statement {
    let (ctes, body_tokens) = parse_ctes(tokens);
    match body_tokens.first() {
        Some(token) if token_is_word(token, "SELECT") => parse_select_with_ctes(ctes, body_tokens)
            .map(Statement::Select)
            .unwrap_or(Statement::Unsupported),
        Some(token) if token_is_word(token, "INSERT") || token_is_word(token, "REPLACE") => {
            parse_insert_with_ctes(ctes, body_tokens)
                .map(Statement::Insert)
                .unwrap_or(Statement::Unsupported)
        }
        Some(token) if token_is_word(token, "UPDATE") => parse_update_with_ctes(ctes, body_tokens)
            .map(Statement::Update)
            .unwrap_or(Statement::Unsupported),
        Some(token) if token_is_word(token, "DELETE") => parse_delete_with_ctes(ctes, body_tokens)
            .map(Statement::Delete)
            .unwrap_or(Statement::Unsupported),
        _ => Statement::Unsupported,
    }
}

fn parse_select(tokens: &[Token]) -> Option<SelectStmt> {
    let (ctes, body_tokens) = parse_ctes(tokens);
    parse_select_with_ctes(ctes, body_tokens)
}

fn parse_select_with_ctes(ctes: Vec<CteDef>, body_tokens: &[Token]) -> Option<SelectStmt> {
    Some(SelectStmt {
        ctes,
        body: parse_select_body(body_tokens)?,
    })
}

fn parse_ctes(tokens: &[Token]) -> (Vec<CteDef>, &[Token]) {
    if !tokens
        .first()
        .is_some_and(|token| token_is_word(token, "WITH"))
    {
        return (Vec::new(), tokens);
    }

    let mut index = 1usize;
    if tokens
        .get(index)
        .is_some_and(|token| token_is_word(token, "RECURSIVE"))
    {
        index += 1;
    }

    let mut ctes = Vec::new();
    while index < tokens.len() {
        let Some(name) = tokens.get(index).and_then(identifier_from_token) else {
            break;
        };
        let name = name.to_ascii_lowercase();
        index += 1;

        let mut columns = Vec::new();
        if matches!(tokens.get(index), Some(Token::OpenParen)) {
            let (column_tokens, after_columns) = collect_balanced_parens(tokens, index);
            columns = split_top_level_commas(column_tokens)
                .into_iter()
                .filter_map(|tokens| tokens.first().and_then(identifier_from_token))
                .map(str::to_ascii_lowercase)
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
        ctes.push(CteDef {
            name,
            columns,
            body: body.to_vec(),
        });
        index = after_body;

        if matches!(tokens.get(index), Some(Token::Comma)) {
            index += 1;
            continue;
        }
        break;
    }

    (ctes, &tokens[index..])
}

fn parse_select_body(tokens: &[Token]) -> Option<SelectBody> {
    if !tokens
        .first()
        .is_some_and(|token| token_is_word(token, "SELECT"))
    {
        return None;
    }

    let mut after_select = &tokens[1..];
    let is_distinct = after_select
        .first()
        .is_some_and(|token| token_is_word(token, "DISTINCT"));
    if is_distinct {
        after_select = &after_select[1..];
    }

    let compound_boundaries = ["UNION", "INTERSECT", "EXCEPT"];
    let (select_list, after_select_list) = take_until_top_level_keywords(
        after_select,
        &[
            "FROM",
            "WHERE",
            "GROUP",
            "HAVING",
            "ORDER",
            "LIMIT",
            "UNION",
            "INTERSECT",
            "EXCEPT",
        ],
    );
    let (from_slice, after_from) = take_clause(
        after_select_list,
        "FROM",
        &[
            "WHERE",
            "GROUP",
            "HAVING",
            "ORDER",
            "LIMIT",
            "UNION",
            "INTERSECT",
            "EXCEPT",
        ],
    );
    let (where_clause, after_where) = take_clause(
        after_from,
        "WHERE",
        &[
            "GROUP",
            "HAVING",
            "ORDER",
            "LIMIT",
            "UNION",
            "INTERSECT",
            "EXCEPT",
        ],
    );
    let (group_by, after_group) = take_clause(
        after_where,
        "GROUP",
        &["HAVING", "ORDER", "LIMIT", "UNION", "INTERSECT", "EXCEPT"],
    );
    let (having, after_having) = take_clause(
        after_group,
        "HAVING",
        &["ORDER", "LIMIT", "UNION", "INTERSECT", "EXCEPT"],
    );
    let (order_by, after_order) = take_clause(
        after_having,
        "ORDER",
        &["LIMIT", "UNION", "INTERSECT", "EXCEPT"],
    );
    let (limit, _) = take_clause(after_order, "LIMIT", &compound_boundaries);

    Some(SelectBody {
        is_distinct,
        select_list: select_list.to_vec(),
        from: from_slice.map(from_items_from_clause).unwrap_or_default(),
        where_clause: where_clause.map(<[Token]>::to_vec),
        group_by: group_by.map(<[Token]>::to_vec),
        having: having.map(<[Token]>::to_vec),
        order_by: order_by.map(<[Token]>::to_vec),
        limit: limit.map(<[Token]>::to_vec),
    })
}

fn parse_insert_with_ctes(ctes: Vec<CteDef>, tokens: &[Token]) -> Option<InsertStmt> {
    let (conflict_action, after_conflict) = parse_insert_conflict_action(tokens);
    if !tokens
        .get(after_conflict)
        .is_some_and(|token| token_is_word(token, "INTO"))
    {
        return None;
    }
    let into_index = after_conflict + 1;
    let (target, after_target) = parse_table_binding(tokens, into_index)?;
    let (column_list, after_columns) = parse_optional_column_list(tokens, after_target);
    let (source_tokens, after_source) = take_until_insert_source_end(&tokens[after_columns..]);
    let source = parse_insert_source(source_tokens);
    let (upsert, after_upsert) = take_clause(after_source, "ON", &["RETURNING"]);
    let (returning, _) = take_clause(after_upsert, "RETURNING", &[]);

    Some(InsertStmt {
        ctes,
        conflict_action,
        target,
        column_list,
        source,
        upsert: upsert.map(<[Token]>::to_vec),
        returning: returning.map(<[Token]>::to_vec),
    })
}

fn parse_optional_column_list(tokens: &[Token], index: usize) -> (Option<Vec<String>>, usize) {
    if !matches!(tokens.get(index), Some(Token::OpenParen)) {
        return (None, index);
    }

    let (column_tokens, after_columns) = collect_balanced_parens(tokens, index);
    let columns = split_top_level_commas(column_tokens)
        .into_iter()
        .filter_map(|tokens| tokens.first().and_then(identifier_from_token))
        .map(str::to_ascii_lowercase)
        .collect();
    (Some(columns), after_columns)
}

fn take_until_insert_source_end(tokens: &[Token]) -> (&[Token], &[Token]) {
    let mut depth = 0usize;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            _ if depth == 0 && token_is_word(token, "RETURNING") => {
                return (&tokens[..index], &tokens[index..]);
            }
            _ if depth == 0
                && token_is_word(token, "ON")
                && tokens
                    .get(index + 1)
                    .is_some_and(|token| token_is_word(token, "CONFLICT")) =>
            {
                return (&tokens[..index], &tokens[index..]);
            }
            _ => {}
        }
    }

    (tokens, &[])
}

fn parse_insert_source(tokens: &[Token]) -> InsertSource {
    match tokens.first() {
        Some(token) if token_is_word(token, "VALUES") => InsertSource::Values {
            raw: tokens.to_vec(),
            rows: parse_values_rows(&tokens[1..]),
        },
        Some(token) if token_is_word(token, "DEFAULT") => {
            if tokens
                .get(1)
                .is_some_and(|token| token_is_word(token, "VALUES"))
            {
                InsertSource::DefaultValues
            } else {
                InsertSource::Values {
                    raw: tokens.to_vec(),
                    rows: Vec::new(),
                }
            }
        }
        Some(token) if token_is_word(token, "SELECT") || token_is_word(token, "WITH") => {
            parse_select(tokens)
                .map(InsertSource::Select)
                .unwrap_or_else(|| InsertSource::Values {
                    raw: tokens.to_vec(),
                    rows: Vec::new(),
                })
        }
        _ => InsertSource::Values {
            raw: tokens.to_vec(),
            rows: Vec::new(),
        },
    }
}

fn parse_values_rows(tokens: &[Token]) -> Vec<Vec<Vec<Token>>> {
    let mut rows = Vec::new();
    let mut index = 0usize;

    while index < tokens.len() {
        if matches!(tokens.get(index), Some(Token::OpenParen)) {
            let (row_tokens, after_row) = collect_balanced_parens(tokens, index);
            rows.push(
                split_top_level_commas(row_tokens)
                    .into_iter()
                    .map(<[Token]>::to_vec)
                    .collect(),
            );
            index = after_row;
            continue;
        }
        index += 1;
    }

    rows
}

fn parse_insert_conflict_action(tokens: &[Token]) -> (InsertConflictAction, usize) {
    match tokens.first() {
        Some(token) if token_is_word(token, "REPLACE") => (InsertConflictAction::Replace, 1),
        Some(token) if token_is_word(token, "INSERT") => match (tokens.get(1), tokens.get(2)) {
            (Some(or), Some(action)) if token_is_word(or, "OR") => {
                match identifier_from_token(action)
                    .map(str::to_ascii_uppercase)
                    .as_deref()
                {
                    Some("REPLACE") => (InsertConflictAction::Replace, 3),
                    Some("IGNORE") => (InsertConflictAction::Ignore, 3),
                    Some("FAIL") => (InsertConflictAction::Fail, 3),
                    Some("ROLLBACK") => (InsertConflictAction::Rollback, 3),
                    Some("ABORT") => (InsertConflictAction::Abort, 3),
                    _ => (InsertConflictAction::Abort, 1),
                }
            }
            _ => (InsertConflictAction::Abort, 1),
        },
        _ => (InsertConflictAction::Abort, 0),
    }
}

fn parse_update_with_ctes(ctes: Vec<CteDef>, tokens: &[Token]) -> Option<UpdateStmt> {
    let after_update = after_update_keyword(tokens)?;
    let (target, after_target) = parse_table_binding(tokens, after_update)?;
    if !tokens
        .get(after_target)
        .is_some_and(|token| token_is_word(token, "SET"))
    {
        return None;
    }

    let after_set = &tokens[after_target + 1..];
    let (set, after_set_body) =
        take_until_top_level_keywords(after_set, &["FROM", "WHERE", "RETURNING"]);
    let (from_slice, after_from) = take_clause(after_set_body, "FROM", &["WHERE", "RETURNING"]);
    let (where_clause, after_where) = take_clause(after_from, "WHERE", &["RETURNING"]);
    let (returning, _) = take_clause(after_where, "RETURNING", &[]);

    Some(UpdateStmt {
        ctes,
        target,
        set: set.to_vec(),
        from: from_slice.map(from_items_from_clause).unwrap_or_default(),
        where_clause: where_clause.map(<[Token]>::to_vec),
        returning: returning.map(<[Token]>::to_vec),
    })
}

fn after_update_keyword(tokens: &[Token]) -> Option<usize> {
    if !tokens
        .first()
        .is_some_and(|token| token_is_word(token, "UPDATE"))
    {
        return None;
    }
    if tokens
        .get(1)
        .is_some_and(|token| token_is_word(token, "OR"))
        && tokens.get(2).and_then(identifier_from_token).is_some()
    {
        return Some(3);
    }
    Some(1)
}

fn take_clause<'a>(
    tokens: &'a [Token],
    keyword: &str,
    boundaries: &[&str],
) -> (Option<&'a [Token]>, &'a [Token]) {
    if !tokens
        .first()
        .is_some_and(|token| token_is_word(token, keyword))
    {
        return (None, tokens);
    }

    let (body, rest) = take_until_top_level_keywords(&tokens[1..], boundaries);
    (Some(body), rest)
}

fn take_until_top_level_keywords<'a>(
    tokens: &'a [Token],
    boundaries: &[&str],
) -> (&'a [Token], &'a [Token]) {
    let mut depth = 0usize;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            _ if depth == 0
                && boundaries
                    .iter()
                    .any(|boundary| token_is_word(token, boundary)) =>
            {
                return (&tokens[..index], &tokens[index..]);
            }
            _ => {}
        }
    }

    (tokens, &[])
}

fn collect_balanced_parens(tokens: &[Token], open_index: usize) -> (&[Token], usize) {
    if !matches!(tokens.get(open_index), Some(Token::OpenParen)) {
        return (&[], open_index);
    }

    let mut depth = 0usize;
    let inner_start = open_index + 1;
    for (index, token) in tokens.iter().enumerate().skip(open_index) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return (&tokens[inner_start..index], index + 1);
                }
            }
            _ => {}
        }
    }

    (&tokens[inner_start..], tokens.len())
}

fn split_top_level_commas(tokens: &[Token]) -> Vec<&[Token]> {
    let mut groups = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                groups.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }

    if start < tokens.len() {
        groups.push(&tokens[start..]);
    }

    groups
}

fn from_items_from_clause(tokens: &[Token]) -> Vec<FromItem> {
    let mut prefixed = Vec::with_capacity(tokens.len() + 1);
    prefixed.push(Token::Word("FROM".to_string()));
    prefixed.extend_from_slice(tokens);
    from_items(&prefixed)
}

fn parse_delete_with_ctes(ctes: Vec<CteDef>, tokens: &[Token]) -> Option<DeleteStmt> {
    if !tokens
        .first()
        .is_some_and(|token| token_is_word(token, "DELETE"))
        || !tokens
            .get(1)
            .is_some_and(|token| token_is_word(token, "FROM"))
    {
        return None;
    }

    let (target, after_target) = parse_table_binding(tokens, 2)?;
    let (where_clause, after_where) = take_clause(&tokens[after_target..], "WHERE", &["RETURNING"]);
    let (returning, _) = take_clause(after_where, "RETURNING", &[]);

    Some(DeleteStmt {
        ctes,
        target,
        where_clause: where_clause.map(<[Token]>::to_vec),
        returning: returning.map(<[Token]>::to_vec),
    })
}

pub fn from_items(tokens: &[Token]) -> Vec<FromItem> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut index = 0usize;

    while index < tokens.len() {
        match &tokens[index] {
            Token::OpenParen => {
                depth += 1;
                index += 1;
            }
            Token::CloseParen => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            token if depth == 0 && token_is_word(token, "FROM") => {
                index = parse_from_sequence(tokens, index + 1, &mut items);
            }
            token if depth == 0 && token_is_word(token, "JOIN") => {
                if let Some((binding, after_binding)) = parse_table_binding(tokens, index + 1) {
                    items.push(FromItem { binding });
                    index = after_binding;
                } else {
                    index += 1;
                }
            }
            token if depth == 0 && compound_keyword(token) => break,
            _ => index += 1,
        }
    }

    items
}

pub fn build_alias_map(bindings: &[TableBinding]) -> Result<BTreeMap<String, String>, AliasError> {
    let mut map = BTreeMap::new();

    for binding in bindings {
        let key = binding
            .alias
            .as_deref()
            .unwrap_or(&binding.table.name)
            .to_ascii_lowercase();
        let table = binding.table.name.to_ascii_lowercase();
        if map.insert(key.clone(), table).is_some() {
            return Err(AliasError::Collision(key));
        }
    }

    Ok(map)
}

pub fn resolve_qualified<'a, C>(
    aliases: &BTreeMap<String, String>,
    qualifier: &str,
    column: &str,
    schema: &'a BTreeMap<String, BTreeMap<String, C>>,
) -> ColumnResolution<&'a C> {
    let Some(table) = aliases.get(&qualifier.to_ascii_lowercase()) else {
        return ColumnResolution::UnknownQualifiedAlias;
    };
    let Some(columns) = schema.get(table) else {
        return ColumnResolution::UnknownTableRef;
    };
    let Some(column) = columns.get(&column.to_ascii_lowercase()) else {
        return ColumnResolution::UnknownColumnInKnownTable;
    };

    ColumnResolution::Resolved(ResolvedColumn {
        table: table.clone(),
        column,
    })
}

pub fn resolve_bare<'a, C>(
    aliases: &BTreeMap<String, String>,
    column: &str,
    schema: &'a BTreeMap<String, BTreeMap<String, C>>,
) -> ColumnResolution<&'a C> {
    let column = column.to_ascii_lowercase();
    let mut unknown_table = false;
    let mut matches = Vec::new();

    for table in aliases.values() {
        let Some(columns) = schema.get(table) else {
            unknown_table = true;
            continue;
        };
        if let Some(schema_column) = columns.get(&column) {
            matches.push(ResolvedColumn {
                table: table.clone(),
                column: schema_column,
            });
        }
    }

    match matches.len() {
        0 if unknown_table => ColumnResolution::UnknownTableRef,
        0 => ColumnResolution::UnknownBareColumn,
        1 => ColumnResolution::Resolved(matches.remove(0)),
        _ => ColumnResolution::AmbiguousColumn,
    }
}

pub fn table_references(tokens: &[Token]) -> BTreeMap<String, String> {
    let mut refs = BTreeMap::new();
    for item in all_from_items(tokens) {
        let key = item
            .binding
            .alias
            .as_deref()
            .unwrap_or(&item.binding.table.name)
            .to_ascii_lowercase();
        refs.insert(key, item.binding.table.name.to_ascii_lowercase());
    }
    refs
}

fn all_from_items(tokens: &[Token]) -> Vec<FromItem> {
    let mut items = Vec::new();
    let mut index = 0usize;

    while index < tokens.len() {
        if tokens
            .get(index)
            .is_some_and(|token| token_is_word(token, "FROM"))
        {
            index = parse_from_sequence(tokens, index + 1, &mut items);
            continue;
        }
        if tokens
            .get(index)
            .is_some_and(|token| token_is_word(token, "JOIN"))
        {
            if let Some((binding, after_binding)) = parse_table_binding(tokens, index + 1) {
                items.push(FromItem { binding });
                index = after_binding;
                continue;
            }
        }
        index += 1;
    }

    items
}

fn parse_from_sequence(tokens: &[Token], mut index: usize, items: &mut Vec<FromItem>) -> usize {
    while index < tokens.len() {
        if matches!(tokens.get(index), Some(Token::Comma)) {
            index += 1;
            continue;
        }
        if tokens.get(index).is_some_and(from_sequence_boundary) {
            break;
        }

        let Some((binding, after_binding)) = parse_table_binding(tokens, index) else {
            break;
        };
        items.push(FromItem { binding });
        index = after_binding;

        while index < tokens.len()
            && !matches!(tokens.get(index), Some(Token::Comma))
            && !tokens.get(index).is_some_and(from_sequence_boundary)
            && !tokens.get(index).is_some_and(join_keyword)
        {
            index += 1;
        }

        if tokens.get(index).is_some_and(join_keyword) {
            break;
        }
    }

    index
}

fn parse_table_binding(tokens: &[Token], index: usize) -> Option<(TableBinding, usize)> {
    let (table, mut index) = parse_table_ref(tokens, index)?;
    let mut alias = None;

    if tokens
        .get(index)
        .is_some_and(|token| token_is_word(token, "AS"))
    {
        if let Some(name) = tokens.get(index + 1).and_then(identifier_from_token)
            && !alias_stop_word(name)
        {
            alias = Some(name.to_ascii_lowercase());
            index += 2;
        }
    } else if let Some(name) = tokens.get(index).and_then(identifier_from_token)
        && !alias_stop_word(name)
    {
        alias = Some(name.to_ascii_lowercase());
        index += 1;
    }

    Some((TableBinding { table, alias }, index))
}

fn parse_table_ref(tokens: &[Token], index: usize) -> Option<(TableRef, usize)> {
    let first = identifier_from_token(tokens.get(index)?)?;
    if matches!(tokens.get(index + 1), Some(Token::Dot)) {
        let name = identifier_from_token(tokens.get(index + 2)?)?;
        return Some((
            TableRef {
                schema: Some(first.to_ascii_lowercase()),
                name: name.to_ascii_lowercase(),
            },
            index + 3,
        ));
    }

    Some((
        TableRef {
            schema: None,
            name: first.to_ascii_lowercase(),
        },
        index + 1,
    ))
}

fn identifier_from_token(token: &Token) -> Option<&str> {
    match token {
        Token::Word(word) | Token::QuotedId(word) => Some(word),
        _ => None,
    }
}

fn token_is_word(token: &Token, expected: &str) -> bool {
    matches!(token, Token::Word(word) if word.eq_ignore_ascii_case(expected))
}

fn from_sequence_boundary(token: &Token) -> bool {
    matches!(token, Token::Semicolon)
        || matches!(
            identifier_from_token(token).map(|word| word.to_ascii_uppercase()),
            Some(word)
                if matches!(
                    word.as_str(),
                    "WHERE"
                        | "GROUP"
                        | "HAVING"
                        | "ORDER"
                        | "LIMIT"
                        | "OFFSET"
                        | "UNION"
                        | "INTERSECT"
                        | "EXCEPT"
                )
        )
}

fn join_keyword(token: &Token) -> bool {
    matches!(
        identifier_from_token(token).map(|word| word.to_ascii_uppercase()),
        Some(word)
            if matches!(
                word.as_str(),
                "JOIN" | "LEFT" | "RIGHT" | "INNER" | "OUTER" | "CROSS" | "NATURAL" | "FULL"
            )
    )
}

fn compound_keyword(token: &Token) -> bool {
    matches!(
        identifier_from_token(token).map(|word| word.to_ascii_uppercase()),
        Some(word) if matches!(word.as_str(), "UNION" | "INTERSECT" | "EXCEPT")
    )
}

fn alias_stop_word(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "ON" | "USING"
            | "WHERE"
            | "GROUP"
            | "HAVING"
            | "ORDER"
            | "LIMIT"
            | "OFFSET"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "JOIN"
            | "LEFT"
            | "RIGHT"
            | "INNER"
            | "OUTER"
            | "CROSS"
            | "NATURAL"
            | "FULL"
            | "INDEXED"
            | "NOT"
            | "RETURNING"
            | "VALUES"
            | "DEFAULT"
            | "SELECT"
            | "WITH"
            | "SET"
            | "FROM"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::tokenize::tokenize;

    fn parse_from(sql: &str) -> Vec<FromItem> {
        from_items(&tokenize(sql))
    }

    fn parse_sql(sql: &str) -> Statement {
        parse_statement(&tokenize(sql))
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestColumn {
        name: &'static str,
        ty: &'static str,
        nullable: bool,
    }

    fn test_schema() -> BTreeMap<String, BTreeMap<String, TestColumn>> {
        BTreeMap::from([
            (
                "users".to_string(),
                BTreeMap::from([
                    (
                        "id".to_string(),
                        TestColumn {
                            name: "id",
                            ty: "integer",
                            nullable: false,
                        },
                    ),
                    (
                        "email".to_string(),
                        TestColumn {
                            name: "email",
                            ty: "text",
                            nullable: false,
                        },
                    ),
                ]),
            ),
            (
                "orders".to_string(),
                BTreeMap::from([
                    (
                        "id".to_string(),
                        TestColumn {
                            name: "id",
                            ty: "integer",
                            nullable: false,
                        },
                    ),
                    (
                        "total".to_string(),
                        TestColumn {
                            name: "total",
                            ty: "integer",
                            nullable: false,
                        },
                    ),
                ]),
            ),
        ])
    }

    #[test]
    fn parses_insert_default_conflict_target() {
        assert_eq!(
            parse_sql("insert into t (a) values (?)"),
            Statement::Insert(InsertStmt {
                ctes: Vec::new(),
                conflict_action: InsertConflictAction::Abort,
                target: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "t".to_string(),
                    },
                    alias: None,
                },
                column_list: Some(vec!["a".to_string()]),
                source: InsertSource::Values {
                    raw: tokenize("values (?)"),
                    rows: vec![vec![vec![Token::ParamAnon]]],
                },
                upsert: None,
                returning: None,
            })
        );
    }

    #[test]
    fn parses_insert_values_source() {
        let Statement::Insert(stmt) = parse_sql("insert into t (a, b) values (?, ?)") else {
            panic!("expected insert statement");
        };

        assert_eq!(
            stmt.column_list,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        let InsertSource::Values { rows, .. } = stmt.source else {
            panic!("expected values source");
        };
        assert_eq!(
            rows,
            vec![vec![vec![Token::ParamAnon], vec![Token::ParamAnon]]]
        );
    }

    #[test]
    fn parses_insert_values_multi_row_source() {
        let Statement::Insert(stmt) = parse_sql("insert into t (a, b) values (?, ?), (?, ?)")
        else {
            panic!("expected insert statement");
        };

        let InsertSource::Values { rows, .. } = stmt.source else {
            panic!("expected values source");
        };
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn parses_insert_values_without_column_list() {
        let Statement::Insert(stmt) = parse_sql("insert into t values (?, ?)") else {
            panic!("expected insert statement");
        };

        assert_eq!(stmt.column_list, None);
        let InsertSource::Values { rows, .. } = stmt.source else {
            panic!("expected values source");
        };
        assert_eq!(
            rows,
            vec![vec![vec![Token::ParamAnon], vec![Token::ParamAnon]]]
        );
    }

    #[test]
    fn parses_insert_default_values() {
        let Statement::Insert(stmt) = parse_sql("insert into t default values") else {
            panic!("expected insert statement");
        };

        assert_eq!(stmt.source, InsertSource::DefaultValues);
    }

    #[test]
    fn parses_insert_select_source() {
        let Statement::Insert(stmt) = parse_sql("insert into t (a) select id from other") else {
            panic!("expected insert statement");
        };

        let InsertSource::Select(select) = stmt.source else {
            panic!("expected select source");
        };
        assert_eq!(select.body.from.len(), 1);
    }

    #[test]
    fn parses_insert_returning_clause() {
        let Statement::Insert(stmt) = parse_sql("insert into t (a) values (?) returning id, name")
        else {
            panic!("expected insert statement");
        };

        assert!(stmt.returning.is_some());
    }

    #[test]
    fn parses_insert_select_with_join_on_without_upsert() {
        let Statement::Insert(stmt) =
            parse_sql("insert into t (a, b) select a.id, b.id from a join b on a.id = b.a_id")
        else {
            panic!("expected insert statement");
        };

        let InsertSource::Select(select) = stmt.source else {
            panic!("expected select source");
        };
        assert_eq!(select.body.from.len(), 2);
        assert!(stmt.upsert.is_none());
    }

    #[test]
    fn parses_insert_on_conflict_upsert() {
        let Statement::Insert(stmt) =
            parse_sql("insert into t (a) values (?) on conflict (a) do update set a = excluded.a")
        else {
            panic!("expected insert statement");
        };

        assert!(stmt.upsert.is_some());
    }

    #[test]
    fn parses_select_simple() {
        let Statement::Select(stmt) = parse_sql("select a, b from t") else {
            panic!("expected select statement");
        };

        assert!(!stmt.body.is_distinct);
        assert!(!stmt.body.select_list.is_empty());
        assert_eq!(
            stmt.body.from,
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "t".to_string(),
                    },
                    alias: None,
                },
            }]
        );
        assert!(stmt.body.where_clause.is_none());
        assert!(stmt.body.group_by.is_none());
        assert!(stmt.body.having.is_none());
        assert!(stmt.body.order_by.is_none());
        assert!(stmt.body.limit.is_none());
    }

    #[test]
    fn parses_select_distinct() {
        let Statement::Select(stmt) = parse_sql("select distinct a from t") else {
            panic!("expected select statement");
        };

        assert!(stmt.body.is_distinct);
    }

    #[test]
    fn parses_select_full_clauses() {
        let Statement::Select(stmt) = parse_sql(
            "select a from t where x = 1 group by a having count(*) > 1 order by a limit 10",
        ) else {
            panic!("expected select statement");
        };

        assert!(stmt.body.where_clause.is_some());
        assert!(stmt.body.group_by.is_some());
        assert!(stmt.body.having.is_some());
        assert!(stmt.body.order_by.is_some());
        assert!(stmt.body.limit.is_some());
    }

    #[test]
    fn parse_select_does_not_split_on_subquery_keyword() {
        let Statement::Select(stmt) = parse_sql("select a from (select a from t where b = 1) sub")
        else {
            panic!("expected select statement");
        };

        assert!(stmt.body.where_clause.is_none());
    }

    #[test]
    fn parses_select_compound_without_aliasing_table_as_compound_keyword() {
        for sql in [
            "select a from t1 union select b from t2",
            "select a from t1 intersect select a from t2",
            "select a from t1 except select a from t2",
        ] {
            let Statement::Select(stmt) = parse_sql(sql) else {
                panic!("expected select statement for {sql}");
            };

            assert_eq!(
                stmt.body.from,
                [FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "t1".to_string(),
                        },
                        alias: None,
                    },
                }]
            );
        }
    }

    #[test]
    fn parses_with_simple_cte() {
        let Statement::Select(stmt) = parse_sql("with foo as (select 1) select * from foo") else {
            panic!("expected select statement");
        };

        assert_eq!(stmt.ctes.len(), 1);
        assert_eq!(stmt.ctes[0].name, "foo");
        assert!(stmt.ctes[0].body.len() > 0);
        assert_eq!(
            stmt.body.from,
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "foo".to_string(),
                    },
                    alias: None,
                },
            }]
        );
    }

    #[test]
    fn parses_with_cte_columns() {
        let Statement::Select(stmt) =
            parse_sql("with foo (a, b) as (select 1, 2) select * from foo")
        else {
            panic!("expected select statement");
        };

        assert_eq!(stmt.ctes[0].columns, ["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parses_with_insert_statement() {
        let Statement::Insert(stmt) =
            parse_sql("with source as (select 1 as a) insert into t (a) select a from source")
        else {
            panic!("expected insert statement");
        };

        assert_eq!(stmt.ctes.len(), 1);
        assert_eq!(stmt.ctes[0].name, "source");
        assert_eq!(stmt.target.table.name, "t");
        assert_eq!(stmt.column_list, Some(vec!["a".to_string()]));
        assert!(matches!(stmt.source, InsertSource::Select(_)));
    }

    #[test]
    fn parses_with_update_statement() {
        let Statement::Update(stmt) = parse_sql(
            "with source as (select 1 as id) update users set name = 'x' where id in (select id from source)",
        ) else {
            panic!("expected update statement");
        };

        assert_eq!(stmt.ctes.len(), 1);
        assert_eq!(stmt.ctes[0].name, "source");
        assert_eq!(stmt.target.table.name, "users");
        assert!(stmt.where_clause.is_some());
    }

    #[test]
    fn parses_with_delete_statement() {
        let Statement::Delete(stmt) = parse_sql(
            "with source as (select 1 as id) delete from users where id in (select id from source)",
        ) else {
            panic!("expected delete statement");
        };

        assert_eq!(stmt.ctes.len(), 1);
        assert_eq!(stmt.ctes[0].name, "source");
        assert_eq!(stmt.target.table.name, "users");
        assert!(stmt.where_clause.is_some());
    }

    #[test]
    fn parses_with_recursive_cte() {
        let Statement::Select(stmt) = parse_sql(
            "with recursive counter(n) as (
                select 1 union all select n+1 from counter where n < 10
            ) select * from counter",
        ) else {
            panic!("expected select statement");
        };

        assert_eq!(stmt.ctes[0].name, "counter");
        assert_eq!(stmt.ctes[0].columns, ["n".to_string()]);
    }

    #[test]
    fn parses_with_multiple_ctes() {
        let Statement::Select(stmt) =
            parse_sql("with a as (select 1), b as (select 2) select * from a, b")
        else {
            panic!("expected select statement");
        };

        assert_eq!(
            stmt.ctes
                .iter()
                .map(|cte| cte.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn parses_insert_conflict_actions() {
        for (sql, expected) in [
            (
                "insert or ignore into t (a) values (?)",
                InsertConflictAction::Ignore,
            ),
            (
                "insert or replace into t (a) values (?)",
                InsertConflictAction::Replace,
            ),
            (
                "insert or fail into t (a) values (?)",
                InsertConflictAction::Fail,
            ),
            (
                "insert or rollback into t (a) values (?)",
                InsertConflictAction::Rollback,
            ),
            (
                "replace into t (a) values (?)",
                InsertConflictAction::Replace,
            ),
        ] {
            let Statement::Insert(stmt) = parse_sql(sql) else {
                panic!("expected insert statement for {sql}");
            };

            assert_eq!(stmt.conflict_action, expected);
        }
    }

    #[test]
    fn parses_insert_target_alias() {
        let Statement::Insert(stmt) = parse_sql("insert into t as target (a) values (?)") else {
            panic!("expected insert statement");
        };

        assert_eq!(stmt.target.alias.as_deref(), Some("target"));
    }

    #[test]
    fn parses_update_simple() {
        let Statement::Update(stmt) = parse_sql("update users set name = ? where id = ?") else {
            panic!("expected update statement");
        };

        assert_eq!(
            stmt.target,
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: None,
            }
        );
        assert!(!stmt.set.is_empty());
        assert!(stmt.where_clause.is_some());
    }

    #[test]
    fn parses_update_target_alias() {
        let Statement::Update(stmt) = parse_sql("update users as u set email = ? where u.id = ?")
        else {
            panic!("expected update statement");
        };

        assert_eq!(stmt.target.alias.as_deref(), Some("u"));
    }

    #[test]
    fn parses_update_from_clause() {
        let Statement::Update(stmt) =
            parse_sql("update users as u set email = o.email from orders o where u.id = o.user_id")
        else {
            panic!("expected update statement");
        };

        assert_eq!(
            stmt.from,
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "orders".to_string(),
                    },
                    alias: Some("o".to_string()),
                },
            }]
        );
        assert!(stmt.where_clause.is_some());
    }

    #[test]
    fn parses_update_returning_clause() {
        let Statement::Update(stmt) =
            parse_sql("update users set name = ? where id = ? returning id, name")
        else {
            panic!("expected update statement");
        };

        assert!(stmt.returning.is_some());
    }

    #[test]
    fn parses_delete_simple() {
        let Statement::Delete(stmt) = parse_sql("delete from users where id = ?") else {
            panic!("expected delete statement");
        };

        assert_eq!(
            stmt.target,
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: None,
            }
        );
        assert!(stmt.where_clause.is_some());
    }

    #[test]
    fn parses_delete_target_alias() {
        let Statement::Delete(stmt) = parse_sql("delete from users as u where u.id = ?") else {
            panic!("expected delete statement");
        };

        assert_eq!(stmt.target.alias.as_deref(), Some("u"));
    }

    #[test]
    fn parses_delete_returning_clause() {
        let Statement::Delete(stmt) = parse_sql("delete from users where id = ? returning id")
        else {
            panic!("expected delete statement");
        };

        assert!(stmt.returning.is_some());
    }

    #[test]
    fn parses_unsupported_statement() {
        assert_eq!(
            parse_sql("create table users (id integer primary key)"),
            Statement::Unsupported
        );
        assert_eq!(
            parse_sql("insert users (name) values (?)"),
            Statement::Unsupported
        );
    }

    #[test]
    fn parses_single_table_from_item() {
        assert_eq!(
            parse_from("select * from users"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "users".to_string(),
                    },
                    alias: None,
                },
            }]
        );
    }

    #[test]
    fn parses_explicit_and_implicit_aliases() {
        assert_eq!(
            parse_from("select * from users as u join orders o on o.user_id = u.id"),
            [
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("u".to_string()),
                    },
                },
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "orders".to_string(),
                        },
                        alias: Some("o".to_string()),
                    },
                },
            ]
        );
    }

    #[test]
    fn parses_schema_qualified_and_quoted_tables() {
        assert_eq!(
            parse_from(r#"select * from main."user table""#),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: Some("main".to_string()),
                        name: "user table".to_string(),
                    },
                    alias: None,
                },
            }]
        );
    }

    #[test]
    fn parses_comma_separated_from_items() {
        assert_eq!(
            parse_from("select * from users u, orders o"),
            [
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("u".to_string()),
                    },
                },
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "orders".to_string(),
                        },
                        alias: Some("o".to_string()),
                    },
                },
            ]
        );
    }

    #[test]
    fn parses_self_join_aliases() {
        assert_eq!(
            parse_from("select * from users u join users manager on manager.id = u.manager_id"),
            [
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("u".to_string()),
                    },
                },
                FromItem {
                    binding: TableBinding {
                        table: TableRef {
                            schema: None,
                            name: "users".to_string(),
                        },
                        alias: Some("manager".to_string()),
                    },
                },
            ]
        );
    }

    #[test]
    fn does_not_treat_clause_keywords_as_aliases() {
        assert_eq!(
            parse_from("select a from t1 union select b from t2"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "t1".to_string(),
                    },
                    alias: None,
                },
            }]
        );
        assert_eq!(
            parse_from("select * from into"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "into".to_string(),
                    },
                    alias: None,
                },
            }]
        );
        assert_eq!(
            parse_from("select * from returning"),
            [FromItem {
                binding: TableBinding {
                    table: TableRef {
                        schema: None,
                        name: "returning".to_string(),
                    },
                    alias: None,
                },
            }]
        );
    }

    #[test]
    fn alias_map_uses_aliases_when_present_and_table_names_otherwise() {
        let bindings = [
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("u".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "orders".to_string(),
                },
                alias: None,
            },
        ];

        let map = build_alias_map(&bindings).unwrap();

        assert_eq!(map.get("u").map(String::as_str), Some("users"));
        assert_eq!(map.get("orders").map(String::as_str), Some("orders"));
        assert!(!map.contains_key("users"));
    }

    #[test]
    fn alias_map_allows_self_joins_with_distinct_aliases() {
        let bindings = [
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("u".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("manager".to_string()),
            },
        ];

        let map = build_alias_map(&bindings).unwrap();

        assert_eq!(map.get("u").map(String::as_str), Some("users"));
        assert_eq!(map.get("manager").map(String::as_str), Some("users"));
        assert!(!map.contains_key("users"));
    }

    #[test]
    fn alias_map_rejects_collisions() {
        let bindings = [
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("x".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "orders".to_string(),
                },
                alias: Some("x".to_string()),
            },
        ];

        assert_eq!(
            build_alias_map(&bindings),
            Err(AliasError::Collision("x".to_string()))
        );
    }

    #[test]
    fn resolves_qualified_known_columns() {
        let bindings = [
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("u".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "orders".to_string(),
                },
                alias: Some("o".to_string()),
            },
        ];
        let aliases = build_alias_map(&bindings).unwrap();
        let schema = test_schema();

        let ColumnResolution::Resolved(column) = resolve_qualified(&aliases, "u", "email", &schema)
        else {
            panic!("expected resolved column");
        };

        assert_eq!(column.table, "users");
        assert_eq!(column.column.name, "email");
    }

    #[test]
    fn reports_qualified_resolution_failures() {
        let bindings = [TableBinding {
            table: TableRef {
                schema: None,
                name: "users".to_string(),
            },
            alias: Some("u".to_string()),
        }];
        let aliases = build_alias_map(&bindings).unwrap();
        let schema = test_schema();

        assert_eq!(
            resolve_qualified(&aliases, "x", "id", &schema),
            ColumnResolution::UnknownQualifiedAlias
        );
        assert_eq!(
            resolve_qualified(&aliases, "u", "nope", &schema),
            ColumnResolution::UnknownColumnInKnownTable
        );

        let unknown_table_aliases = build_alias_map(&[TableBinding {
            table: TableRef {
                schema: None,
                name: "foo_cte".to_string(),
            },
            alias: None,
        }])
        .unwrap();
        assert_eq!(
            resolve_qualified(&unknown_table_aliases, "foo_cte", "x", &schema),
            ColumnResolution::UnknownTableRef
        );
    }

    #[test]
    fn resolves_bare_columns() {
        let aliases = build_alias_map(&[TableBinding {
            table: TableRef {
                schema: None,
                name: "users".to_string(),
            },
            alias: Some("u".to_string()),
        }])
        .unwrap();
        let schema = test_schema();

        let ColumnResolution::Resolved(column) = resolve_bare(&aliases, "email", &schema) else {
            panic!("expected resolved column");
        };

        assert_eq!(column.table, "users");
        assert_eq!(column.column.name, "email");
    }

    #[test]
    fn reports_bare_resolution_failures() {
        let schema = test_schema();
        let aliases = build_alias_map(&[
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("u".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "orders".to_string(),
                },
                alias: Some("o".to_string()),
            },
        ])
        .unwrap();

        assert_eq!(
            resolve_bare(&aliases, "id", &schema),
            ColumnResolution::AmbiguousColumn
        );
        assert_eq!(
            resolve_bare(&aliases, "missing", &schema),
            ColumnResolution::UnknownBareColumn
        );

        let unknown_table_aliases = build_alias_map(&[
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "users".to_string(),
                },
                alias: Some("u".to_string()),
            },
            TableBinding {
                table: TableRef {
                    schema: None,
                    name: "foo_cte".to_string(),
                },
                alias: None,
            },
        ])
        .unwrap();
        assert_eq!(
            resolve_bare(&unknown_table_aliases, "x", &schema),
            ColumnResolution::UnknownTableRef
        );
    }

    #[test]
    fn table_references_include_nested_query_bindings_for_analyzer_inference() {
        let refs = table_references(&tokenize(
            "select id from users where exists (
                select 1 from audit_logs u where u.actor_id = @actor_id
            )",
        ));

        assert_eq!(refs.get("users").map(String::as_str), Some("users"));
        assert_eq!(refs.get("u").map(String::as_str), Some("audit_logs"));
    }
}
