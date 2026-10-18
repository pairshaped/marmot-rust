use super::*;
use crate::sqlite::parse::{InsertSource, Statement, parse_statement};
use crate::value_sets::{ValueSet, invalid};

pub(super) fn validate(
    sql: &str,
    schema: &Schema,
    path: &Path,
) -> Result<BTreeMap<String, ValueSet>> {
    if !schema
        .tables
        .values()
        .flat_map(|columns| columns.values())
        .any(|column| matches!(column.column_type, ValueType::ValueSet(_)))
    {
        return Ok(BTreeMap::new());
    }
    let tokens = tokenize(sql);
    let schema = schema_with_ctes(&tokens, schema);
    let mut validator = Validator {
        schema: &schema,
        refs: local_refs(&tokens),
        parameters: BTreeMap::new(),
    };
    // Each pass can bind another parameter in a comparison chain. The map only
    // grows; conflicting evidence fails rather than replacing the first type.
    loop {
        let before = validator.parameters.len();
        validator
            .statement(&tokens)
            .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
        if validator.parameters.len() == before {
            break;
        }
    }
    Ok(validator.parameters)
}

struct Validator<'a> {
    schema: &'a Schema,
    refs: BTreeMap<String, String>,
    parameters: BTreeMap<String, ValueSet>,
}

impl Validator<'_> {
    fn statement(&mut self, tokens: &[Token]) -> Result<()> {
        if top_level(tokens).iter().any(|(_, token)| {
            ["UNION", "EXCEPT", "INTERSECT"]
                .iter()
                .any(|keyword| token_is_word(token, keyword))
        }) {
            let outer_refs = self.refs.clone();
            self.refs = table_references(tokens);
            let result = self.reject_typed(tokens);
            self.refs = outer_refs;
            result?;
        }
        let statement = parse_statement(tokens);
        let ctes = match &statement {
            Statement::Select(value) => &value.ctes,
            Statement::Insert(value) => &value.ctes,
            Statement::Update(value) => &value.ctes,
            Statement::Delete(value) => &value.ctes,
            Statement::Unsupported => &Vec::new(),
        };
        for cte in ctes {
            self.nested(&cte.body)?;
        }
        match statement {
            Statement::Select(select) => {
                for (_, body) in derived_table_bodies(tokens) {
                    self.nested(body)?;
                }
                for expression in split_top_level_commas(&select.body.select_list) {
                    self.expression(expression_without_alias(expression), None)?;
                }
                for expression in [
                    select.body.where_clause,
                    select.body.having,
                    select.body.group_by,
                    select.body.order_by,
                ]
                .into_iter()
                .flatten()
                {
                    for part in split_top_level_commas(&expression) {
                        self.expression(part, None)?;
                    }
                }
            }
            Statement::Update(update) => {
                for assignment in split_top_level_commas(&update.set) {
                    self.expression(assignment, None)?;
                }
                if let Some(condition) = update.where_clause {
                    self.expression(&condition, None)?;
                }
                if let Some(returning) = update.returning {
                    for part in split_top_level_commas(&returning) {
                        self.expression(expression_without_alias(part), None)?;
                    }
                }
            }
            Statement::Insert(insert) => {
                let table = insert.target.table.name.to_ascii_lowercase();
                let columns = insert
                    .column_list
                    .unwrap_or_else(|| self.schema.column_names_in_table_order(&table));
                match insert.source {
                    InsertSource::Values { rows, .. } => {
                        for row in rows {
                            for (expression, column) in row.iter().zip(&columns) {
                                let expected = self
                                    .schema
                                    .column(&table, column)
                                    .and_then(|column| as_set(&column.column_type))
                                    .cloned();
                                self.expression(expression, expected.as_ref())?;
                            }
                        }
                    }
                    InsertSource::Select(_) => {
                        // The existing analyzer understands INSERT SELECT params,
                        // but assigning whole expression lists needs column proof.
                        if columns.iter().any(|column| {
                            self.schema
                                .column(&table, column)
                                .is_some_and(|column| as_set(&column.column_type).is_some())
                        }) {
                            return Err(invalid(
                                "INSERT SELECT into a declared column is ambiguous; use typed VALUES",
                            ));
                        }
                    }
                    InsertSource::DefaultValues => (),
                }
                if let Some(upsert) = insert.upsert {
                    self.reject_typed(&upsert)?;
                }
                if let Some(returning) = insert.returning {
                    for part in split_top_level_commas(&returning) {
                        self.expression(expression_without_alias(part), None)?;
                    }
                }
            }
            Statement::Delete(delete) => {
                if let Some(condition) = delete.where_clause {
                    self.expression(&condition, None)?;
                }
                if let Some(returning) = delete.returning {
                    for part in split_top_level_commas(&returning) {
                        self.expression(expression_without_alias(part), None)?;
                    }
                }
            }
            Statement::Unsupported => self.reject_typed(tokens)?,
        }
        // The statement model keeps relation bindings but not ON expressions.
        for (index, token) in tokens.iter().enumerate() {
            if !token_is_word(token, "ON") {
                continue;
            }
            let end = tokens
                .iter()
                .enumerate()
                .skip(index + 1)
                .find_map(|(i, token)| {
                    (join_keyword(token) || from_sequence_boundary(token)).then_some(i)
                })
                .unwrap_or(tokens.len());
            self.expression(&tokens[index + 1..end], None)?;
        }
        Ok(())
    }

    fn nested(&mut self, tokens: &[Token]) -> Result<()> {
        let outer_refs = self.refs.clone();
        self.refs = local_refs(tokens);
        let result = self.statement(tokens);
        self.refs = outer_refs;
        result
    }

    fn column(&self, tokens: &[Token]) -> Result<Option<ValueSet>> {
        let Some(column) = column_ref_from_expression(tokens) else {
            return Ok(None);
        };
        if column.qualifier.is_none() {
            let matches = self
                .refs
                .values()
                .filter_map(|table| self.schema.column(table, &column.column))
                .collect::<Vec<_>>();
            if matches.len() > 1
                && matches
                    .iter()
                    .any(|column| as_set(&column.column_type).is_some())
            {
                return Err(invalid(format!(
                    "ambiguous column {}: qualify its table",
                    column.column
                )));
            }
        }
        Ok(resolve_column_ref(self.schema, &self.refs, &column)
            .and_then(|column| as_set(&column.column_type))
            .cloned())
    }

    fn expression(
        &mut self,
        tokens: &[Token],
        expected: Option<&ValueSet>,
    ) -> Result<Option<ValueSet>> {
        let mut tokens = tokens;
        while matches!(tokens.first(), Some(Token::OpenParen))
            && matching_open_paren(tokens, tokens.len() - 1) == Some(0)
        {
            tokens = &tokens[1..tokens.len() - 1];
        }
        if tokens.is_empty() {
            return Ok(None);
        }
        if token_is_word(&tokens[0], "SELECT") || token_is_word(&tokens[0], "WITH") {
            let outer_refs = self.refs.clone();
            self.refs.extend(table_references(tokens));
            let result = self.reject_typed(tokens);
            self.refs = outer_refs;
            result?;
            return Ok(None);
        }
        if token_is_word(&tokens[0], "BY") {
            return self.expression(&tokens[1..], expected);
        }
        if tokens
            .last()
            .is_some_and(|token| token_is_word(token, "ASC") || token_is_word(token, "DESC"))
        {
            return self.expression(&tokens[..tokens.len() - 1], expected);
        }
        if let Some(Token::Word(function)) = tokens.first()
            && matches!(
                function.to_ascii_uppercase().as_str(),
                "ROW_NUMBER" | "RANK" | "DENSE_RANK"
            )
            && let Some(over) = tokens.iter().position(|token| token_is_word(token, "OVER"))
            && matches!(tokens.get(over + 1), Some(Token::OpenParen))
            && matches!(tokens.last(), Some(Token::CloseParen))
        {
            if expected.is_some() {
                return Err(invalid(
                    "a window rank cannot produce a declared text value",
                ));
            }
            let window = &tokens[over + 2..tokens.len() - 1];
            for keyword in ["PARTITION", "ORDER"] {
                if let Some(start) = top_level_keyword(window, keyword) {
                    let end = if keyword == "PARTITION" {
                        top_level_keyword(window, "ORDER").unwrap_or(window.len())
                    } else {
                        window.len()
                    };
                    for part in split_top_level_commas(&window[start + 2..end]) {
                        self.expression(part, None)?;
                    }
                }
            }
            return Ok(None);
        }
        if token_is_word(&tokens[0], "CASE") {
            return self.case(tokens, expected);
        }
        for (index, token) in top_level(tokens) {
            if token_is_word(token, "AND") || token_is_word(token, "OR") {
                if expected.is_some() {
                    return Err(invalid("a predicate cannot produce a declared text value"));
                }
                self.expression(&tokens[..index], None)?;
                self.expression(&tokens[index + 1..], None)?;
                return Ok(None);
            }
        }
        for (index, token) in top_level(tokens) {
            if comparison_operator(token) || token_is_word(token, "IS") {
                if expected.is_some() {
                    return Err(invalid("a comparison cannot produce a declared text value"));
                }
                let left = self.expression(&tokens[..index], None)?;
                let right_tokens = if tokens
                    .get(index + 1)
                    .is_some_and(|token| token_is_word(token, "NOT"))
                {
                    &tokens[index + 2..]
                } else {
                    &tokens[index + 1..]
                };
                let right = self.expression(right_tokens, left.as_ref())?;
                self.expression(&tokens[..index], right.as_ref())?;
                return Ok(None);
            }
            if token_is_word(token, "IN") {
                if expected.is_some() {
                    return Err(invalid("IN cannot produce a declared text value"));
                }
                let left_end = if index > 0 && token_is_word(&tokens[index - 1], "NOT") {
                    index - 1
                } else {
                    index
                };
                let set = self.expression(&tokens[..left_end], None)?;
                if matches!(tokens.get(index + 1), Some(Token::OpenParen))
                    && matches!(tokens.last(), Some(Token::CloseParen))
                {
                    for item in split_top_level_commas(&tokens[index + 2..tokens.len() - 1]) {
                        let item_set = self.expression(item, set.as_ref())?;
                        self.expression(&tokens[..left_end], item_set.as_ref())?;
                    }
                    return Ok(None);
                }
            }
        }
        let actual = self.column(tokens)?;
        if actual.is_some() {
            compatible(actual.as_ref(), expected)?;
            return Ok(actual);
        }
        if column_ref_from_expression(tokens).is_some() && expected.is_none() {
            return Ok(None);
        }
        if let [Token::ParamNamed { prefix, name }] = tokens {
            let key = format!("{prefix}{name}");
            let actual = self.parameters.get(&key).cloned();
            compatible(actual.as_ref(), expected)?;
            if let Some(set) = expected {
                self.parameters.insert(key, set.clone());
            }
            return Ok(actual.or_else(|| expected.cloned()));
        }
        if let [Token::StringLit(value)] = tokens {
            if let Some(set) = expected
                && !set.values.contains(value)
            {
                return Err(invalid(format!(
                    "literal {value:?} is outside {}",
                    set.name
                )));
            }
            return Ok(expected.cloned());
        }
        if tokens.len() == 1 && token_is_word(&tokens[0], "NULL") {
            return Ok(expected.cloned());
        }
        if expected.is_some() {
            return Err(invalid(format!(
                "ambiguous expression {tokens:?}; use a declared column, known literal or typed named parameter"
            )));
        }
        self.reject_typed(tokens)?;
        Ok(None)
    }

    fn case(&mut self, tokens: &[Token], expected: Option<&ValueSet>) -> Result<Option<ValueSet>> {
        let Some(end) = matching_case_end(tokens, 0) else {
            return Err(invalid("unclosed CASE"));
        };
        if end + 1 != tokens.len() {
            self.reject_typed(tokens)?;
            return Ok(None);
        }
        let first_when = top_level_case_words(tokens)
            .into_iter()
            .find(|(_, word)| word.eq_ignore_ascii_case("WHEN"))
            .map(|(index, _)| index)
            .ok_or_else(|| invalid("CASE requires WHEN"))?;
        let operand = self.expression(&tokens[1..first_when], None)?;
        let words = top_level_case_words(tokens);
        let mut branches = Vec::new();
        for (position, (index, word)) in words.iter().enumerate() {
            let next = words
                .get(position + 1)
                .map(|(index, _)| *index)
                .unwrap_or(end);
            if word.eq_ignore_ascii_case("WHEN") {
                self.expression(&tokens[index + 1..next], operand.as_ref())?;
            }
            if word.eq_ignore_ascii_case("THEN") || word.eq_ignore_ascii_case("ELSE") {
                branches.push(&tokens[index + 1..next]);
            }
        }
        let mut result = expected.cloned();
        for branch in &branches {
            if let Some(set) = self.expression(branch, None)? {
                compatible(result.as_ref(), Some(&set))?;
                result = Some(set);
            }
        }
        for branch in branches {
            self.expression(branch, result.as_ref())?;
        }
        Ok(result)
    }

    fn reject_typed(&self, tokens: &[Token]) -> Result<()> {
        for index in 0..tokens.len() {
            if index > 0 && matches!(tokens[index - 1], Token::Dot) {
                continue;
            }
            if let Some(column) = column_ref_starting_at(tokens, index) {
                let length = if column.qualifier.is_some() { 3 } else { 1 };
                if self
                    .column(&tokens[index..(index + length).min(tokens.len())])?
                    .is_some()
                {
                    return Err(invalid(format!(
                        "unsupported expression {tokens:?} involving declared column {}; return the column and transform it in Rust",
                        column.column
                    )));
                }
            }
            if let Token::ParamNamed { prefix, name } = &tokens[index]
                && self.parameters.contains_key(&format!("{prefix}{name}"))
            {
                return Err(invalid(
                    "unsupported expression involving a typed parameter",
                ));
            }
        }
        Ok(())
    }
}

fn as_set(kind: &ValueType) -> Option<&ValueSet> {
    if let ValueType::ValueSet(set) = kind {
        Some(set)
    } else {
        None
    }
}

fn compatible(left: Option<&ValueSet>, right: Option<&ValueSet>) -> Result<()> {
    if let (Some(left), Some(right)) = (left, right)
        && left != right
    {
        return Err(invalid(format!(
            "conflicting {} and {} column identities",
            left.name, right.name
        )));
    }
    Ok(())
}

fn top_level(tokens: &[Token]) -> Vec<(usize, &Token)> {
    let mut depth = 0usize;
    let mut cases = 0usize;
    let mut result = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            _ if token_is_word(token, "CASE") => cases += 1,
            _ if token_is_word(token, "END") => cases = cases.saturating_sub(1),
            _ if depth == 0 && cases == 0 => result.push((index, token)),
            _ => (),
        }
    }
    result
}

fn top_level_case_words(tokens: &[Token]) -> Vec<(usize, &str)> {
    let mut depth = 0usize;
    let mut result = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token_is_word(token, "CASE") {
            depth += 1;
        } else if token_is_word(token, "END") {
            depth -= 1;
        } else if depth == 1
            && let Token::Word(word) = token
            && matches!(word.to_ascii_uppercase().as_str(), "WHEN" | "THEN" | "ELSE")
        {
            result.push((index, word.as_str()));
        }
    }
    result
}

fn local_refs(tokens: &[Token]) -> BTreeMap<String, String> {
    let mut refs = BTreeMap::new();
    match parse_statement(tokens) {
        Statement::Select(select) => {
            for item in &select.body.from {
                add_table_binding_ref(&mut refs, &item.binding);
            }
        }
        Statement::Update(update) => {
            add_table_binding_ref(&mut refs, &update.target);
            for item in &update.from {
                add_table_binding_ref(&mut refs, &item.binding);
            }
        }
        Statement::Insert(insert) => add_table_binding_ref(&mut refs, &insert.target),
        Statement::Delete(delete) => add_table_binding_ref(&mut refs, &delete.target),
        Statement::Unsupported => refs = table_references(tokens),
    }
    refs
}

pub(super) fn annotate_views(conn: &Connection, schema: &mut Schema, path: &Path) -> Result<()> {
    if !schema
        .tables
        .values()
        .flat_map(|columns| columns.values())
        .any(|column| as_set(&column.column_type).is_some())
    {
        return Ok(());
    }
    let mut statement = conn
        .prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'view'")
        .map_err(|source| Error::InspectDatabase { source })?;
    let views = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|source| Error::InspectDatabase { source })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|source| Error::InspectDatabase { source })?;
    loop {
        let mut changed = false;
        for (name, sql) in &views {
            let spans = tokenize_spans(sql);
            let start = spans
                .iter()
                .find(|token| token_is_word(&token.token, "SELECT"))
                .map(|token| token.start)
                .ok_or_else(|| invalid(format!("view {name} has no SELECT")))?;
            let body = &sql[start..];
            validate(body, schema, path)?;
            let view_query = format!("SELECT * FROM \"{}\"", name.replace('"', "\"\""));
            let (mut columns, _) = result_columns(conn, schema, path, &view_query, &view_query)?;
            let inferred =
                select_expression_inferences(body, schema, &outer_join_nullable_tables(body));
            for (index, column) in columns.iter_mut().enumerate() {
                if let Some(inference) = inferred.by_index.get(index).and_then(Option::as_ref)
                    && let Some(kind @ ValueType::ValueSet(_)) = &inference.column_type
                {
                    column.column_type = kind.clone();
                    column.nullable = inference.inferred_nullable.unwrap_or(column.nullable);
                }
            }
            let names = schema.column_names_in_table_order(name);
            for (column, name_column) in columns.iter().zip(names) {
                if let Some(set) = as_set(&column.column_type) {
                    let target = schema
                        .tables
                        .get_mut(&name.to_ascii_lowercase())
                        .and_then(|columns| columns.get_mut(&name_column))
                        .expect("view columns came from schema");
                    if target.column_type != column.column_type
                        || target.nullable != column.nullable
                    {
                        target.column_type = ValueType::ValueSet(set.clone());
                        target.nullable = column.nullable;
                        target.notnull = !column.nullable;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            return Ok(());
        }
    }
}
