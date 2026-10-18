use super::*;
use crate::sqlite::parse::{InsertSource, Statement, parse_statement};
use crate::value_sets::{ValueSet, invalid};

#[derive(Default)]
pub(super) struct Validation {
    pub parameters: BTreeMap<String, ValueSet>,
    pub columns: Vec<Option<ValueSet>>,
}

pub(super) fn validate(sql: &str, schema: &Schema, path: &Path) -> Result<Validation> {
    if !schema
        .tables
        .values()
        .flat_map(|columns| columns.values())
        .any(|column| matches!(column.column_type, ValueType::ValueSet(_)))
    {
        return Ok(Validation::default());
    }
    let tokens = tokenize(sql);
    let mut validator = Validator {
        schema: schema.clone(),
        refs: local_refs(&tokens),
        parameters: BTreeMap::new(),
        compound_results: Vec::new(),
    };
    // Each pass can bind another parameter in a comparison chain. The map only
    // grows; conflicting evidence fails rather than replacing the first type.
    let columns = loop {
        let before = validator.parameters.len();
        validator.compound_results.clear();
        let result = validator.statement(&tokens);
        if validator.parameters.len() == before {
            break result.map_err(|error| invalid(format!("{}: {error}", path.display())))?;
        }
        // A later arm may have supplied the parameter identity needed by a
        // subsequent consumer of its CTE. Recheck errors with stable evidence.
    };
    // A later arm can bind a parameter used by an earlier result. The last
    // fixed-point pass has stable identities, so only its result proof is kept.
    for arms in &validator.compound_results {
        for current in &arms[1..] {
            for (left, right) in arms[0].iter().zip(current) {
                compatible(left.as_ref(), right.as_ref())
                    .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
                if left.is_some() != right.is_some() {
                    return Err(invalid(format!(
                        "{}: compound enum results require the same declared set in every arm",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(Validation {
        parameters: validator.parameters,
        columns,
    })
}

struct Validator {
    schema: Schema,
    refs: BTreeMap<String, String>,
    parameters: BTreeMap<String, ValueSet>,
    compound_results: Vec<Vec<Vec<Option<ValueSet>>>>,
}

impl Validator {
    fn statement(&mut self, tokens: &[Token]) -> Result<Vec<Option<ValueSet>>> {
        let statement = parse_statement(tokens);
        add_ctes_to_schema(tokens, &mut self.schema);
        add_derived_tables_to_schema(tokens, &mut self.schema);
        let ctes = match &statement {
            Statement::Select(value) => &value.ctes,
            Statement::Insert(value) => &value.ctes,
            Statement::Update(value) => &value.ctes,
            Statement::Delete(value) => &value.ctes,
            Statement::Unsupported => &Vec::new(),
        };
        for cte in ctes {
            self.relation(&cte.name, &cte.columns, &cte.body)?;
        }
        if matches!(statement, Statement::Select(_)) {
            for (alias, body) in derived_table_bodies(tokens) {
                self.relation(&alias, &[], body)?;
            }
        }
        let arms = top_level_compound_arms(tokens);
        if matches!(statement, Statement::Select(_)) && arms.len() > 1 {
            let has_declared_source = arms.iter().any(|arm| {
                local_refs(arm).values().any(|table| {
                    self.schema.tables.get(table).is_some_and(|columns| {
                        columns
                            .values()
                            .any(|column| as_set(&column.column_type).is_some())
                    })
                })
            });
            if has_declared_source
                && arms.iter().any(|arm| {
                    top_level_select_list(arm).is_some_and(|list| {
                        split_top_level_commas(list)
                            .iter()
                            .any(|projection| wildcard_projection(projection))
                    })
                })
            {
                return Err(invalid(
                    "compound declared-column queries require explicit result projections",
                ));
            }
            let mut results = Vec::new();
            for arm in arms {
                results.push(self.nested(arm)?);
            }
            let first = results[0].clone();
            self.compound_results.push(results);
            return Ok(first);
        }
        let mut result = Vec::new();
        let source_has_own_scope = matches!(&statement, Statement::Insert(insert) if matches!(insert.source, InsertSource::Select(_)));
        match statement {
            Statement::Select(select) => {
                for expression in split_top_level_commas(&select.body.select_list) {
                    result.push(self.expression(expression_without_alias(expression), None)?);
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
                    InsertSource::Select(select) => {
                        let source = insert_select_tokens(tokens)
                            .ok_or_else(|| invalid("unresolved INSERT SELECT source"))?;
                        if top_level_compound_arms(source).len() > 1
                            && columns.iter().any(|column| {
                                self.schema
                                    .column(&table, column)
                                    .is_some_and(|column| as_set(&column.column_type).is_some())
                            })
                        {
                            return Err(invalid(
                                "compound INSERT SELECT into declared columns is unsupported",
                            ));
                        }
                        self.nested(source)?;
                        let outer_refs = std::mem::replace(&mut self.refs, local_refs(source));
                        let assignment = (|| {
                            let projections = split_top_level_commas(&select.body.select_list);
                            if projections.len() != columns.len()
                                && columns.iter().any(|column| {
                                    self.schema
                                        .column(&table, column)
                                        .is_some_and(|column| as_set(&column.column_type).is_some())
                                })
                            {
                                return Err(invalid(
                                    "INSERT SELECT declared-column assignments require explicit projections",
                                ));
                            }
                            for (projection, column) in
                                split_top_level_commas(&select.body.select_list)
                                    .iter()
                                    .zip(&columns)
                            {
                                let expected = self
                                    .schema
                                    .column(&table, column)
                                    .and_then(|column| as_set(&column.column_type))
                                    .cloned();
                                let Some(expected) = expected else {
                                    continue;
                                };
                                let projection = expression_without_alias(projection);
                                let direct = column_ref_from_expression(projection).is_some()
                                    || matches!(
                                        projection,
                                        [Token::StringLit(_) | Token::ParamNamed { .. }]
                                    )
                                    || (projection.len() == 1
                                        && token_is_word(&projection[0], "NULL"));
                                if !direct {
                                    return Err(invalid(
                                        "INSERT SELECT enum assignments require a known literal, typed parameter or same-set column",
                                    ));
                                }
                                self.expression(projection, Some(&expected))?;
                            }
                            Ok(())
                        })();
                        self.refs = outer_refs;
                        assignment?;
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
        if !source_has_own_scope {
            self.joins(tokens)?;
        }
        Ok(result)
    }

    fn joins(&mut self, tokens: &[Token]) -> Result<()> {
        // The statement model keeps relation bindings but not ON expressions.
        let top = top_level(tokens);
        for (position, (index, token)) in top.iter().enumerate() {
            if !token_is_word(token, "ON") {
                continue;
            }
            let end = top
                .iter()
                .skip(position + 1)
                .find_map(|(i, token)| {
                    (join_keyword(token) || from_sequence_boundary(token)).then_some(*i)
                })
                .unwrap_or(tokens.len());
            self.expression(&tokens[index + 1..end], None)?;
        }
        Ok(())
    }

    fn nested(&mut self, tokens: &[Token]) -> Result<Vec<Option<ValueSet>>> {
        let outer_refs = std::mem::replace(&mut self.refs, local_refs(tokens));
        let outer_schema = self.schema.clone();
        let result = self.statement(tokens);
        self.refs = outer_refs;
        self.schema = outer_schema;
        result
    }

    fn relation(&mut self, name: &str, names: &[String], body: &[Token]) -> Result<()> {
        let proofs = self.nested(body)?;
        if let Some(mut columns) = infer_cte_columns(body, names, &self.schema) {
            for column in columns.values_mut() {
                if let Some(Some(set)) = proofs.get(column.ordinal as usize) {
                    column.column_type = ValueType::ValueSet(set.clone());
                }
            }
            self.schema
                .tables
                .insert(name.to_ascii_lowercase(), columns);
        }
        Ok(())
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
        Ok(resolve_column_ref(&self.schema, &self.refs, &column)
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
            if expected.is_some() {
                return Err(invalid(
                    "scalar subquery result has no proven value set; use a typed named parameter or a known literal",
                ));
            }
            if top_level_compound_arms(tokens).iter().any(|arm| {
                top_level_select_list(arm).is_some_and(|list| {
                    split_top_level_commas(list)
                        .iter()
                        .any(|projection| wildcard_projection(projection))
                })
            }) {
                return Err(invalid(
                    "scalar subqueries require explicit result projections",
                ));
            }
            let outer_refs = self.refs.clone();
            let outer_schema = self.schema.clone();
            self.refs.extend(local_refs(tokens));
            let result = self.statement(tokens);
            self.refs = outer_refs;
            self.schema = outer_schema;
            if result?.iter().any(Option::is_some) {
                return Err(invalid("scalar subquery enum results are unsupported"));
            }
            return Ok(None);
        }
        if let [Token::Word(function), Token::OpenParen, ..] = tokens {
            if matches!(
                function.to_ascii_uppercase().as_str(),
                "JSON_OBJECT" | "JSON_ARRAY" | "JSON_GROUP_ARRAY"
            ) && matching_open_paren(tokens, tokens.len() - 1) == Some(1)
            {
                if expected.is_some() {
                    return Err(invalid("JSON packing returns text, not a declared enum"));
                }
                for argument in split_top_level_commas(&tokens[2..tokens.len() - 1]) {
                    self.expression(argument, None)?;
                }
                return Ok(None);
            }
            if function.eq_ignore_ascii_case("CAST")
                && matching_open_paren(tokens, tokens.len() - 1) == Some(1)
            {
                let inner = &tokens[2..tokens.len() - 1];
                if let Some(as_index) = top_level_keyword(inner, "AS") {
                    let value = &inner[..as_index];
                    if matches!(inner.get(as_index + 1), Some(token) if token_is_word(token, "TEXT"))
                        && as_index + 2 == inner.len()
                        && matches!(value.first(), Some(Token::Word(name)) if matches!(name.to_ascii_uppercase().as_str(), "JSON_OBJECT" | "JSON_ARRAY" | "JSON_GROUP_ARRAY"))
                    {
                        return self.expression(value, expected);
                    }
                }
            }
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

    fn reject_typed(&mut self, tokens: &[Token]) -> Result<()> {
        let mut index = 0;
        while index < tokens.len() {
            if matches!(tokens[index], Token::OpenParen)
                && tokens.get(index + 1).is_some_and(|token| {
                    token_is_word(token, "SELECT") || token_is_word(token, "WITH")
                })
            {
                let (body, after) = collect_balanced_parens(tokens, index);
                self.expression(body, None)?;
                index = after;
                continue;
            }
            if index > 0 && matches!(tokens[index - 1], Token::Dot) {
                index += 1;
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
            index += 1;
        }
        Ok(())
    }
}

fn wildcard_projection(tokens: &[Token]) -> bool {
    let tokens = if tokens
        .first()
        .is_some_and(|token| token_is_word(token, "DISTINCT") || token_is_word(token, "ALL"))
    {
        &tokens[1..]
    } else {
        tokens
    };
    matches!(tokens, [Token::Star] | [_, Token::Dot, Token::Star])
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

fn insert_select_tokens(tokens: &[Token]) -> Option<&[Token]> {
    let top = top_level(tokens);
    let insert = top
        .iter()
        .position(|(_, token)| token_is_word(token, "INSERT"))?;
    let start = top.iter().skip(insert + 1).find_map(|(index, token)| {
        (token_is_word(token, "SELECT") || token_is_word(token, "WITH")).then_some(*index)
    })?;
    let end = top
        .iter()
        .find_map(|(index, token)| {
            (*index > start
                && (token_is_word(token, "RETURNING")
                    || (token_is_word(token, "ON")
                        && tokens
                            .get(index + 1)
                            .is_some_and(|next| token_is_word(next, "CONFLICT")))))
            .then_some(*index)
        })
        .unwrap_or(tokens.len());
    Some(&tokens[start..end])
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
    for (alias, _) in derived_table_bodies(tokens) {
        refs.insert(alias.to_ascii_lowercase(), alias.to_ascii_lowercase());
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
