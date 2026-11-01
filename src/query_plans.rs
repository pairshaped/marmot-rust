//! Inspect raw SQL without executing its application statement.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use serde::Serialize;

use crate::sqlite::{
    parse,
    tokenize::{SpannedToken, Token, tokenize, tokenize_spans},
};

fn fixed_source(name: &str) -> bool {
    matches!(
        name,
        "json_each"
            | "json_tree"
            | "sqlite_schema"
            | "sqlite_master"
            | "sqlite_temp_schema"
            | "sqlite_temp_master"
    ) || name.starts_with("pragma_")
}

fn identifier(token: &Token) -> Option<&str> {
    match token {
        Token::Word(name) | Token::QuotedId(name) => Some(name),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanStep {
    pub id: i64,
    pub parent: i64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatementPlan {
    pub source_hash: String,
    pub steps: Vec<PlanStep>,
    pub finding_counts: BTreeMap<String, usize>,
    pub growing_scans: BTreeSet<String>,
    pub temporary_sorts: Vec<String>,
    pub unresolved_reads: BTreeSet<String>,
    pub growing_output: BTreeSet<String>,
}

#[derive(Debug)]
struct Catalog {
    tables: BTreeSet<String>,
    fts5: BTreeSet<String>,
    indexes: BTreeMap<String, String>,
    unique_keys: BTreeMap<String, Vec<String>>,
    primary_keys: BTreeMap<String, Vec<String>>,
    views: BTreeMap<String, String>,
}

struct Aliases {
    tables: BTreeMap<String, BTreeSet<String>>,
    qualified: BTreeMap<String, BTreeSet<bool>>,
    source_hash: String,
    sources: Vec<SqlSource>,
}

struct SqlSource {
    sql: String,
    tokens: Vec<Token>,
    spans: Vec<SpannedToken>,
}

struct KeyRead<'a> {
    source: &'a str,
    table: Option<&'a str>,
    columns: &'a [String],
    sole_root_read: bool,
}

impl Catalog {
    fn load(connection: &Connection) -> rusqlite::Result<Self> {
        let mut catalog = Self {
            tables: BTreeSet::new(),
            fts5: BTreeSet::new(),
            indexes: BTreeMap::new(),
            unique_keys: BTreeMap::new(),
            primary_keys: BTreeMap::new(),
            views: BTreeMap::new(),
        };
        let mut statement = connection.prepare("select lower(name),type,lower(tbl_name),sql from main.sqlite_schema union all select lower(name),type,lower(tbl_name),sql from temp.sqlite_schema")?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })? {
            let (name, kind, table, sql) = row?;
            match kind.as_str() {
                "table" => {
                    if sql.as_deref().is_some_and(|sql| tokenize(sql).windows(2).any(|tokens| matches!(tokens, [crate::sqlite::tokenize::Token::Word(using), crate::sqlite::tokenize::Token::Word(engine)] if using.eq_ignore_ascii_case("using") && engine.eq_ignore_ascii_case("fts5")))) {
                        catalog.fts5.insert(name.clone());
                    }
                    catalog.tables.insert(name);
                }
                "index" => {
                    catalog.indexes.insert(name, table);
                }
                "view" => {
                    catalog.views.insert(name, sql.unwrap_or_default());
                }
                _ => {}
            }
        }
        for table in &catalog.tables {
            let mut keys = connection.prepare(
                "select lower(name) from pragma_table_xinfo(?1) where pk > 0 order by pk",
            )?;
            catalog.primary_keys.insert(
                table.clone(),
                keys.query_map([table], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?,
            );
            let mut indexes =
                connection.prepare("select name from pragma_index_list(?1) where \"unique\"=1")?;
            for index in indexes.query_map([table], |row| row.get::<_, String>(0))? {
                let index = index?;
                let mut columns = connection.prepare(
                    "select lower(name) from pragma_index_xinfo(?1) where key=1 order by seqno",
                )?;
                let columns = columns
                    .query_map([&index], |row| row.get::<_, Option<String>>(0))?
                    .collect::<rusqlite::Result<Option<Vec<_>>>>()?;
                if let Some(columns) = columns {
                    catalog
                        .unique_keys
                        .insert(index.to_ascii_lowercase(), columns);
                }
            }
        }
        Ok(catalog)
    }

    fn aliases(&self, sql: &str) -> Aliases {
        let mut aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut qualified: BTreeMap<String, BTreeSet<bool>> = BTreeMap::new();
        let mut pending = vec![sql.to_owned()];
        let mut visited = BTreeSet::new();
        let mut sources = Vec::new();
        while let Some(sql) = pending.pop() {
            let tokens = tokenize(&sql);
            sources.push(SqlSource {
                spans: tokenize_spans(&sql),
                sql: sql.clone(),
                tokens: tokens.clone(),
            });
            // Binding discovery must stop at the enclosing parenthesis. Keep
            // this traversal separate from Marmot's type-inference parser.
            let bounded = tokens
                .iter()
                .map(|token| {
                    if *token == Token::CloseParen {
                        Token::Semicolon
                    } else {
                        token.clone()
                    }
                })
                .collect::<Vec<_>>();
            let mut bindings = parse::all_from_items(&bounded)
                .into_iter()
                .map(|item| item.binding)
                .collect::<Vec<_>>();
            match parse::parse_statement(&tokens) {
                parse::Statement::Update(update) => bindings.push(update.target),
                parse::Statement::Insert(insert) => bindings.push(insert.target),
                _ => {}
            }
            let function_names = bindings
                .iter()
                .filter(|binding| fixed_source(&binding.table.name))
                .map(|binding| binding.table.name.clone())
                .collect::<BTreeSet<_>>();
            for (index, token) in tokens.iter().enumerate() {
                let Some(name) = identifier(token).map(str::to_ascii_lowercase) else {
                    continue;
                };
                if !function_names.contains(&name)
                    || tokens.get(index + 1) != Some(&Token::OpenParen)
                {
                    continue;
                }
                let mut depth = 0usize;
                let mut end = index + 1;
                while end < tokens.len() {
                    match tokens[end] {
                        Token::OpenParen => depth += 1,
                        Token::CloseParen => {
                            depth -= 1;
                            if depth == 0 {
                                end += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    end += 1;
                }
                if tokens
                    .get(end)
                    .and_then(identifier)
                    .is_some_and(|word| word.eq_ignore_ascii_case("as"))
                {
                    end += 1;
                }
                aliases
                    .entry(name.clone())
                    .or_default()
                    .insert(name.clone());
                if let Some(alias) = tokens.get(end).and_then(identifier)
                    && !matches!(
                        alias.to_ascii_lowercase().as_str(),
                        "on" | "where"
                            | "join"
                            | "left"
                            | "right"
                            | "cross"
                            | "group"
                            | "order"
                            | "limit"
                            | "union"
                            | "having"
                            | "returning"
                    )
                {
                    aliases
                        .entry(alias.to_ascii_lowercase())
                        .or_default()
                        .insert(name);
                }
            }
            for binding in bindings {
                let name = binding.alias.clone().unwrap_or_else(|| {
                    binding
                        .table
                        .schema
                        .as_ref()
                        .map(|schema| format!("{schema}.{}", binding.table.name))
                        .unwrap_or_else(|| binding.table.name.clone())
                });
                qualified
                    .entry(name.clone())
                    .or_default()
                    .insert(binding.table.schema.is_some());
                let table = binding.table.name;
                aliases.entry(name).or_default().insert(table.clone());
                if let Some(view) = self.views.get(&table)
                    && visited.insert(table)
                {
                    pending.push(view.clone());
                }
            }
        }
        let dependencies: Vec<_> = visited
            .iter()
            .map(|name| (name, self.views.get(name).map(|sql| normalized_tokens(sql))))
            .collect();
        let hash = digest(
            &serde_json::to_vec(&(normalized_tokens(sql), dependencies))
                .expect("serialize SQL source tokens"),
        );
        Aliases {
            tables: aliases,
            qualified,
            source_hash: hash,
            sources,
        }
    }

    fn point_key(&self, table: &str, detail: &str) -> Option<Vec<String>> {
        let detail = detail.strip_suffix(" LEFT-JOIN").unwrap_or(detail);
        if !detail.starts_with("SEARCH ") {
            return None;
        }
        if detail.contains("USING INTEGER PRIMARY KEY (rowid=?)") {
            let mut keys = vec!["rowid".into(), "_rowid_".into(), "oid".into()];
            if let Some(primary) = self.primary_keys.get(table).filter(|keys| keys.len() == 1) {
                keys.extend(primary.clone());
            }
            return Some(keys);
        }
        let key = if detail.contains("USING PRIMARY KEY (") {
            self.primary_keys.get(table)
        } else {
            let lowered = detail.to_ascii_lowercase();
            self.unique_keys
                .iter()
                .find(|(index, _)| lowered.contains(&format!("index {index} (")))
                .map(|(_, key)| key)
        }?;
        let (_, constraints) = detail.rsplit_once(" (")?;
        let equalities = constraints
            .strip_suffix(')')?
            .split(" AND ")
            .filter_map(|constraint| constraint.strip_suffix("=?"))
            .map(str::to_ascii_lowercase)
            .collect::<BTreeSet<_>>();
        (!key.is_empty() && key.iter().all(|column| equalities.contains(column)))
            .then(|| key.clone())
    }

    fn read_table(
        &self,
        detail: &str,
        aliases: &BTreeMap<String, BTreeSet<String>>,
    ) -> Option<String> {
        let lowered = detail.to_ascii_lowercase();
        // An actual index name identifies its table even when nested scopes reuse aliases.
        for (index, table) in &self.indexes {
            if lowered.ends_with(&format!("index {index}"))
                || lowered.contains(&format!("index {index} ("))
            {
                return Some(table.clone());
            }
        }
        let source = read_source(detail, &self.tables, aliases)?.to_ascii_lowercase();
        if let Some(table) = source
            .strip_prefix("main.")
            .or_else(|| source.strip_prefix("temp."))
        {
            return self.tables.contains(table).then(|| table.to_owned());
        }

        if let Some(tables) = aliases.get(&source) {
            if tables.len() == 1 {
                let table = tables.first().unwrap();
                if self.tables.contains(table) {
                    return Some(table.clone());
                }
            }
            return None;
        }
        self.tables.contains(&source).then_some(source)
    }
}

fn matching_paren(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => {
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

fn scope_start(tokens: &[Token], index: usize) -> Option<usize> {
    let mut closing = 0usize;
    for start in (0..index).rev() {
        match &tokens[start] {
            Token::CloseParen => closing += 1,
            Token::OpenParen if closing > 0 => closing -= 1,
            Token::Word(word)
                if closing == 0
                    && ["select", "update", "delete"]
                        .iter()
                        .any(|kind| word.eq_ignore_ascii_case(kind)) =>
            {
                return Some(start);
            }
            _ => {}
        }
    }
    None
}

fn root_scope_start(tokens: &[Token]) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => depth = depth.saturating_sub(1),
            Token::Word(word)
                if depth == 0
                    && ["select", "update", "delete"]
                        .iter()
                        .any(|kind| word.eq_ignore_ascii_case(kind)) =>
            {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

fn scope_bindings(tokens: &[Token], start: usize) -> Vec<parse::TableBinding> {
    match parse::parse_statement(&tokens[start..]) {
        parse::Statement::Select(select) => select
            .body
            .from
            .into_iter()
            .map(|item| item.binding)
            .collect(),
        parse::Statement::Update(update) => vec![update.target],
        parse::Statement::Delete(delete) => vec![delete.target],
        _ => Vec::new(),
    }
}

// A unique-key probe is bounded per outer row, except when its own IN
// operand supplies a growing set. Prepare that SQL to prove its cardinality.
fn key_has_growing_in(
    sources: &[SqlSource],
    read: KeyRead<'_>,
    fixed: &BTreeSet<String>,
    catalog: &Catalog,
    mut rhs_can_grow: impl FnMut(&str) -> bool,
) -> bool {
    let source = read.source.rsplit('.').next().unwrap_or(read.source);
    let cte_names = sources
        .iter()
        .flat_map(|source| match parse::parse_statement(&source.tokens) {
            parse::Statement::Select(select) => select
                .ctes
                .into_iter()
                .map(|cte| cte.name)
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<BTreeSet<_>>();
    sources.iter().any(|sql_source| {
        let tokens = &sql_source.tokens;
        let root = root_scope_start(tokens);
        let roots = root
            .map(|start| scope_bindings(tokens, start))
            .unwrap_or_default();
        let root_owner = read
            .sole_root_read
            .then(|| {
                roots.iter().find(|binding| {
                    binding
                        .alias
                        .as_deref()
                        .unwrap_or(&binding.table.name)
                        .eq_ignore_ascii_case(source)
                        && catalog.tables.contains(&binding.table.name)
                        && !cte_names.contains(&binding.table.name)
                        && read.table.is_none_or(|table| table == binding.table.name)
                })
            })
            .flatten();
        let table = read
            .table
            .or_else(|| root_owner.map(|binding| binding.table.name.as_str()));
        tokens
            .iter()
            .enumerate()
            .filter(
                |(_, token)| matches!(token, Token::Word(word) if word.eq_ignore_ascii_case("in")),
            )
            .any(|(index, _)| {
                let owner_scope = scope_start(tokens, index);
                if root_owner.is_some() && owner_scope != root {
                    return false;
                }
                let owners = owner_scope
                    .map(|start| scope_bindings(tokens, start))
                    .unwrap_or_default();
                let matching_owners = owners
                    .iter()
                    .filter(|binding| {
                        binding
                            .alias
                            .as_deref()
                            .unwrap_or(&binding.table.name)
                            .eq_ignore_ascii_case(source)
                    })
                    .collect::<Vec<_>>();
                if let Some(table) = table
                    && !matching_owners.is_empty()
                    && matching_owners.iter().all(|binding| {
                        catalog.tables.contains(&binding.table.name)
                            && !cte_names.contains(&binding.table.name)
                            && binding.table.name != table
                    })
                {
                    return false;
                }
                let operands = if tokens.get(index.wrapping_sub(1)) == Some(&Token::CloseParen) {
                    (0..index)
                        .rev()
                        .find(|open| {
                            tokens[*open] == Token::OpenParen
                                && matching_paren(tokens, *open) == Some(index - 1)
                        })
                        .map(|open| &tokens[open + 1..index - 1])
                        .unwrap_or_default()
                } else {
                    &tokens[index.saturating_sub(3)..index]
                };
                let matches = operands
                    .split(|token| *token == Token::Comma)
                    .any(|operand| {
                        let Some(column) = operand.last().and_then(identifier) else {
                            return false;
                        };
                        if !read
                            .columns
                            .iter()
                            .any(|key| key.eq_ignore_ascii_case(column))
                        {
                            return false;
                        }
                        if operand.len() >= 3 && operand[operand.len() - 2] == Token::Dot {
                            operand
                                .get(operand.len() - 3)
                                .and_then(identifier)
                                .is_some_and(|qualifier| qualifier.eq_ignore_ascii_case(source))
                        } else {
                            owners.is_empty()
                                || !matching_owners.is_empty()
                                || owners
                                    .iter()
                                    .any(|binding| !catalog.tables.contains(&binding.table.name))
                        }
                    });
                if !matches {
                    return false;
                }
                if tokens.get(index + 1) != Some(&Token::OpenParen) {
                    return tokens
                        .get(index + 1)
                        .and_then(identifier)
                        .is_none_or(|name| {
                            cte_names.contains(name)
                                || (!fixed.contains(name) && !fixed_source(name))
                        });
                }
                let Some(end) = matching_paren(tokens, index + 1) else {
                    return true;
                };
                let body = &tokens[index + 2..end];
                if !body.first().and_then(identifier).is_some_and(|word| {
                    ["select", "with", "values"]
                        .iter()
                        .any(|kind| word.eq_ignore_ascii_case(kind))
                }) {
                    return false;
                }
                let start = sql_source.spans[index + 2].start;
                let end = sql_source.spans[end - 1].end;
                rhs_can_grow(&sql_source.sql[start..end])
            })
    })
}

fn read_source<'a>(
    detail: &'a str,
    tables: &BTreeSet<String>,
    aliases: &BTreeMap<String, BTreeSet<String>>,
) -> Option<&'a str> {
    let detail = detail.strip_suffix(" LEFT-JOIN").unwrap_or(detail);
    let source = detail
        .strip_prefix("SCAN ")
        .or_else(|| detail.strip_prefix("SEARCH "))?;
    let source = source.strip_prefix("TABLE ").unwrap_or(source);
    let source = source
        .split(" USING ")
        .next()
        .unwrap()
        .split(" VIRTUAL TABLE ")
        .next()
        .unwrap()
        .trim();
    // SQLite 3.51 marks an EXISTS probe before USING. Keep real quoted
    // identifiers ending in EXISTS when the SQL catalog identifies them.
    if let Some(base) = source.strip_suffix(" EXISTS")
        && !aliases.contains_key(&source.to_ascii_lowercase())
        && (aliases.contains_key(&base.to_ascii_lowercase())
            || !tables.contains(&source.to_ascii_lowercase()))
    {
        Some(base)
    } else {
        Some(source)
    }
}

#[derive(Debug)]
enum ReadInput {
    Table(String),
    Candidates(BTreeSet<String>),
    Materialized(Vec<i64>),
    Point,
    Constant,
    Unknown,
}

struct Inputs<'a> {
    steps: &'a [PlanStep],
    reads: BTreeMap<i64, ReadInput>,
    fixed: &'a BTreeSet<String>,
}

impl Inputs<'_> {
    fn tables(&self, id: i64, visited: &mut BTreeSet<i64>) -> BTreeSet<String> {
        match self.reads.get(&id) {
            Some(ReadInput::Table(table)) => BTreeSet::from([table.clone()]),
            Some(ReadInput::Candidates(tables)) => tables.clone(),
            Some(ReadInput::Materialized(parents)) => {
                let possibilities = parents
                    .iter()
                    .map(|parent| self.scope_tables(*parent, &mut visited.clone()))
                    .collect::<Vec<_>>();
                let grows = possibilities
                    .iter()
                    .map(|tables| tables.iter().any(|table| !self.fixed.contains(table)))
                    .collect::<BTreeSet<_>>();
                if grows.len() > 1 {
                    BTreeSet::new()
                } else {
                    possibilities.into_iter().flatten().collect()
                }
            }
            _ => BTreeSet::new(),
        }
    }

    fn scope_tables(&self, parent: i64, visited: &mut BTreeSet<i64>) -> BTreeSet<String> {
        if !visited.insert(parent) {
            return BTreeSet::new();
        }
        let mut tables = BTreeSet::new();
        for step in self
            .steps
            .iter()
            .filter(|step| step.parent == parent && step.id != parent)
        {
            if self.reads.contains_key(&step.id) {
                tables.extend(self.tables(step.id, visited));
            } else if !step.detail.contains("SCALAR SUBQUERY")
                && !step.detail.contains("LIST SUBQUERY")
                && materialized_name(&step.detail).is_none()
            {
                tables.extend(self.scope_tables(step.id, visited));
            }
        }
        tables
    }
}

fn materialized_name(detail: &str) -> Option<&str> {
    detail
        .strip_prefix("MATERIALIZE ")
        .or_else(|| detail.strip_prefix("CO-ROUTINE "))
}

pub(crate) fn normalized_tokens(sql: &str) -> Vec<String> {
    tokenize(sql)
        .iter()
        .map(|token| match token {
            Token::Word(word) => format!("Word({:?})", word.to_ascii_lowercase()),
            _ => format!("{token:?}"),
        })
        .collect()
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

pub fn explain_statement(
    connection: &Connection,
    sql: &str,
    fixed_tables: &BTreeSet<String>,
) -> rusqlite::Result<StatementPlan> {
    PlanInspector::new(connection)?.explain(sql, fixed_tables)
}

/// Reuse schema metadata while inspecting statements against an unchanged schema.
pub struct PlanInspector<'a> {
    connection: &'a Connection,
    catalog: Catalog,
}

impl<'a> PlanInspector<'a> {
    pub fn new(connection: &'a Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            connection,
            catalog: Catalog::load(connection)?,
        })
    }

    pub fn explain(
        &self,
        sql: &str,
        fixed_tables: &BTreeSet<String>,
    ) -> rusqlite::Result<StatementPlan> {
        let sql = crate::analyzer::strip_nullability_overrides(sql);
        let mut statement = self
            .connection
            .prepare(&format!("explain query plan {sql}"))?;
        let steps: Vec<PlanStep> = statement
            .raw_query()
            .mapped(|row| {
                Ok(PlanStep {
                    id: row.get(0)?,
                    parent: row.get(1)?,
                    detail: row.get(3)?,
                })
            })
            .collect::<rusqlite::Result<_>>()?;
        let catalog = &self.catalog;
        let bindings = catalog.aliases(&sql);
        let aliases = &bindings.tables;
        let sources = &bindings.sources;
        let source_hash = bindings.source_hash.clone();
        let parents = steps
            .iter()
            .map(|step| (step.id, step.parent))
            .collect::<BTreeMap<_, _>>();
        let definition_for = |source: &str, step: &PlanStep| {
            if bindings
                .qualified
                .get(source)
                .is_some_and(|qualifications| qualifications == &BTreeSet::from([true]))
            {
                return None;
            }
            let mut names = aliases.get(source).cloned().unwrap_or_default();
            names.insert(source.to_owned());
            let mut scope = step.parent;
            let mut ancestors = BTreeSet::new();
            let mut recursive_ancestors = BTreeSet::new();
            let mut recursive = false;
            while ancestors.insert(scope) {
                recursive |= steps
                    .iter()
                    .any(|step| step.id == scope && step.detail == "RECURSIVE STEP");
                if recursive {
                    recursive_ancestors.insert(scope);
                }
                let Some(parent) = parents.get(&scope) else {
                    break;
                };
                scope = *parent;
            }
            scope = step.parent;
            let mut visited = BTreeSet::new();
            while visited.insert(scope) {
                let candidates = steps
                    .iter()
                    .filter(|definition| {
                        definition.parent == scope
                            && (!ancestors.contains(&definition.id)
                                || recursive_ancestors.contains(&definition.id))
                            && materialized_name(&definition.detail)
                                .is_some_and(|name| names.contains(name))
                    })
                    .map(|definition| definition.id)
                    .collect::<Vec<_>>();
                if !candidates.is_empty() {
                    return Some(candidates);
                }
                let Some(parent) = parents.get(&scope) else {
                    break;
                };
                scope = *parent;
            }
            let shared = steps
                .iter()
                .filter(|definition| {
                    (!ancestors.contains(&definition.id)
                        || recursive_ancestors.contains(&definition.id))
                        && materialized_name(&definition.detail)
                            .is_some_and(|name| names.contains(name))
                })
                .map(|definition| definition.id)
                .collect::<Vec<_>>();
            (!shared.is_empty()).then_some(shared)
        };
        let mut unresolved_reads = BTreeSet::new();
        let mut reads = BTreeMap::new();
        let exists_probe = |step: &PlanStep| {
            read_source(&step.detail, &catalog.tables, aliases).is_some_and(|source| {
                step.detail
                    .starts_with(&format!("SEARCH {source} EXISTS USING "))
            })
        };
        for step in &steps {
            let Some(source) = read_source(&step.detail, &catalog.tables, aliases) else {
                continue;
            };
            let lowered = step.detail.to_ascii_lowercase();
            let indexed = catalog.indexes.keys().any(|index| {
                lowered.ends_with(&format!("index {index}"))
                    || lowered.contains(&format!("index {index} ("))
            });
            let rowid_point = step.detail.starts_with("SEARCH ")
                && step.detail.contains("USING INTEGER PRIMARY KEY (rowid=?)");
            let physical = catalog.read_table(&step.detail, aliases);
            let mut rowid_keys = physical
                .as_ref()
                .and_then(|table| catalog.point_key(table, &step.detail))
                .unwrap_or_else(|| vec!["rowid".into(), "_rowid_".into(), "oid".into()]);
            if let Some(tables) = aliases.get(source.rsplit('.').next().unwrap_or(source)) {
                for table in tables {
                    if let Some(key) = catalog.primary_keys.get(table).filter(|key| key.len() == 1)
                    {
                        rowid_keys.extend(key.clone());
                    }
                }
            }
            let definition = if indexed {
                None
            } else {
                definition_for(source, step)
            };
            let sole_root_read = step.parent == 0
                && steps
                    .iter()
                    .filter(|other| {
                        other.parent == 0
                            && !exists_probe(other)
                            && read_source(&other.detail, &catalog.tables, aliases) == Some(source)
                    })
                    .count()
                    == 1;
            let key_grows = |columns: &[String]| {
                key_has_growing_in(
                    sources,
                    KeyRead {
                        source,
                        table: physical.as_deref(),
                        columns,
                        sole_root_read,
                    },
                    fixed_tables,
                    catalog,
                    |rhs| {
                        !self.explain(rhs, fixed_tables).is_ok_and(|plan| {
                            plan.growing_output.is_empty() && plan.unresolved_reads.is_empty()
                        })
                    },
                )
            };
            let input = if (exists_probe(step) && (physical.is_some() || definition.is_some()))
                || (rowid_point && !key_grows(&rowid_keys))
            {
                ReadInput::Point
            } else if !indexed
                && bindings
                    .qualified
                    .get(source)
                    .is_some_and(|qualifications| qualifications.len() > 1)
            {
                unresolved_reads.insert(step.detail.clone());
                ReadInput::Unknown
            } else if let Some(parent) = definition {
                ReadInput::Materialized(parent)
            } else if let Some(table) = &physical {
                if catalog
                    .point_key(table, &step.detail)
                    .is_some_and(|key| !key_grows(&key))
                {
                    ReadInput::Point
                } else {
                    ReadInput::Table(table.clone())
                }
            } else if let Some(candidates) = aliases.get(source).filter(|targets| {
                targets.len() > 1
                    && targets.iter().all(|target| catalog.tables.contains(target))
                    && (targets.iter().all(|target| fixed_tables.contains(target))
                        || targets.iter().all(|target| !fixed_tables.contains(target)))
            }) {
                ReadInput::Candidates(candidates.clone())
            } else if fixed_source(&source.to_ascii_lowercase())
                || aliases.get(source).is_some_and(|targets| {
                    !targets.is_empty() && targets.iter().all(|target| fixed_source(target))
                })
                || step.detail.contains("CONSTANT ROW")
            {
                ReadInput::Constant
            } else {
                unresolved_reads.insert(step.detail.clone());
                ReadInput::Unknown
            };
            reads.insert(step.id, input);
        }
        let inputs = Inputs {
            steps: &steps,
            reads,
            fixed: fixed_tables,
        };
        for step in &steps {
            if let Some(ReadInput::Materialized(parents)) = inputs.reads.get(&step.id) {
                let classifications = parents
                    .iter()
                    .map(|parent| {
                        inputs
                            .scope_tables(*parent, &mut BTreeSet::new())
                            .iter()
                            .any(|table| !fixed_tables.contains(table))
                    })
                    .collect::<BTreeSet<_>>();
                if classifications.len() > 1 {
                    unresolved_reads.insert(step.detail.clone());
                }
            }
        }
        let scan_steps = steps
        .iter()
        .filter(|step| step.detail.starts_with("SCAN "))
        .filter(|step| !matches!(inputs.reads.get(&step.id),Some(ReadInput::Table(table)) if catalog.fts5.contains(table) && step.detail.split_once(" VIRTUAL TABLE INDEX ").and_then(|(_,index)|index.split_once(':')).is_some_and(|(_,constraints)|!constraints.is_empty())))
        .filter(|step| {
            inputs
                .tables(step.id, &mut BTreeSet::new())
                .iter()
                .any(|table| !fixed_tables.contains(table))
        })
        .collect::<Vec<_>>();
        let mut finding_counts: BTreeMap<String, usize> = BTreeMap::new();
        let growing_scans = scan_steps
            .iter()
            .filter_map(|step| match inputs.reads.get(&step.id) {
                Some(ReadInput::Table(table)) => Some(table.clone()),
                _ => read_source(&step.detail, &catalog.tables, aliases).map(str::to_owned),
            })
            .collect();
        for step in &scan_steps {
            let tables = inputs.tables(step.id, &mut BTreeSet::new());
            let key = format!("scan:{tables:?}:{}", step.detail);
            *finding_counts.entry(key).or_default() += 1;
        }
        let temporary_sorts = steps
            .iter()
            .filter(|step| {
                (step.detail.contains("USE TEMP B-TREE")
                    && (step.detail.contains("ORDER BY") || step.detail.contains("GROUP BY")))
                    || step.detail == "UNION USING TEMP B-TREE"
            })
            .filter(|sort| {
                inputs
                    .scope_tables(sort.parent, &mut BTreeSet::new())
                    .iter()
                    .any(|table| !fixed_tables.contains(table))
            })
            .map(|step| {
                let tables = inputs.scope_tables(step.parent, &mut BTreeSet::new());
                *finding_counts
                    .entry(format!("sort:{tables:?}:{}", step.detail))
                    .or_default() += 1;
                step.detail.clone()
            })
            .collect::<Vec<_>>();
        for step in &steps {
            if unresolved_reads.contains(&step.detail) {
                *finding_counts
                    .entry(format!("unresolved:{}", step.detail))
                    .or_default() += 1;
            }
        }

        let growing_output = inputs
            .scope_tables(0, &mut BTreeSet::new())
            .into_iter()
            .filter(|table| !fixed_tables.contains(table))
            .collect();
        Ok(StatementPlan {
            source_hash,
            finding_counts,
            steps,
            growing_scans,
            temporary_sorts,
            unresolved_reads,
            growing_output,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_plans_resolve_ctes_that_shadow_fixed_or_growing_tables() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key, org_id integer, name text); create index participant_org on participants(org_id); create table countries(code text);").unwrap();
        let fixed = BTreeSet::from(["countries".into()]);
        let growing = explain_statement(&connection,
            "with countries as materialized (select p.name from participants p where p.org_id=@org_id) select name from countries order by name", &fixed).unwrap();
        assert_eq!(growing.temporary_sorts.len(), 1, "{growing:?}");
        let bounded = explain_statement(&connection,
            "with participants as materialized (select code from countries) select code from participants order by code", &fixed).unwrap();
        assert!(bounded.temporary_sorts.is_empty(), "{bounded:?}");
        let growing = explain_statement(&connection,
            "with participants as materialized (select code from countries) select main.participants.name, (select count(*) from participants) from main.participants order by main.participants.name", &fixed).unwrap();
        assert!(
            growing.growing_scans.contains("participants"),
            "{growing:?}"
        );
        assert_eq!(growing.temporary_sorts.len(), 1, "{growing:?}");
        assert!(bounded.growing_scans.is_empty(), "{bounded:?}");
        let bounded = explain_statement(&connection,
            "with countries as materialized (select name from participants) select c.code, (select count(*) from countries) from main.countries c order by c.code", &fixed).unwrap();
        assert!(bounded.temporary_sorts.is_empty(), "{bounded:?}");
    }

    #[test]
    fn query_plans_do_not_pull_an_unrelated_list_into_a_fixed_sort() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key, name text); create table countries(code text);").unwrap();
        let plan = explain_statement(&connection,
            "select c.code from countries c join participants p on p.id=@id where c.code in (select p2.name from participants p2) order by c.code",
            &BTreeSet::from(["countries".into()])).unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
        assert!(plan.growing_scans.contains("participants"), "{plan:?}");
        let plan = explain_statement(&connection,
            "select c.code from countries c join participants p on p.id in (select value from json_each(@ids)) where c.code in (select p2.name from participants p2) order by c.code",
            &BTreeSet::from(["countries".into()])).unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
    }

    #[test]
    fn query_plans_keep_nested_alias_reuse_and_bounded_in_inputs_out_of_fixed_sorts() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table countries(code text); create table participants(id integer primary key, email text unique); create table memberships(email text unique);").unwrap();
        let plan = explain_statement(&connection,
            "select c.code from countries c join participants p on p.email=@email where exists (select 1 from memberships p where p.email in (select email from participants where id=@id)) order by c.code",
            &BTreeSet::from(["countries".into()])).unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
        let fixed = BTreeSet::from(["countries".into()]);
        let plan = explain_statement(&connection,
            "select c.code from countries c join participants p on p.email=@email where exists (select 1 from participants p where p.email in (select email from memberships)) order by c.code", &fixed).unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
        let plan = explain_statement(&connection,
            "select email from memberships where email in (select email from participants where id=@id) order by length(email)", &fixed).unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
        for sql in [
            "select email from (select p.email from participants p where p.email in (select email from memberships)) order by length(email)",
            "select email from main.participants where id in (select rowid from memberships) order by length(email)",
        ] {
            let plan = explain_statement(&connection, sql, &fixed).unwrap();
            assert_eq!(plan.temporary_sorts.len(), 1, "{plan:?}");
        }
    }

    #[test]
    fn query_plans_recognize_unique_and_without_rowid_point_lookups() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key, email text unique, name text); create table memberships(org_id integer, participant_id integer, name text, primary key(org_id, participant_id)) without rowid;").unwrap();
        for sql in [
            "with selected as materialized (select name from participants where email=@email) select name from selected order by name",
            "with selected as materialized (select name from memberships where org_id=@org_id and participant_id=@participant_id) select name from selected order by name",
        ] {
            let plan = explain_statement(&connection, sql, &BTreeSet::new()).unwrap();
            assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
            assert!(plan.growing_scans.is_empty(), "{plan:?}");
        }
        let plan = explain_statement(&connection,
            "select name from participants where email in (select name from memberships) order by name", &BTreeSet::new()).unwrap();
        assert_eq!(plan.temporary_sorts.len(), 1, "{plan:?}");
    }

    #[test]
    fn query_plans_reject_growing_table_scan_even_when_aliased() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("create table participants(id integer primary key, name text);")
            .unwrap();
        let plan = explain_statement(
            &connection,
            "select p.id from participants p where p.name=@name",
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            plan.growing_scans,
            BTreeSet::from(["participants".to_string()])
        );
    }
    #[test]
    fn query_plans_do_not_confuse_a_fixed_scan_with_a_growing_min_lookup() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key); create table countries(code text);").unwrap();
        let plan = explain_statement(
            &connection,
            "select countries.code, (select min(p.id) from participants p) from countries",
            &BTreeSet::from(["countries".to_string()]),
        )
        .unwrap();
        assert!(plan.growing_scans.is_empty(), "{plan:?}");
    }
    #[test]
    fn query_plans_attribute_sorts_to_their_input_branch() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key, org_id integer, name text); create index participant_org on participants(org_id); create table countries(code text); ").unwrap();
        let fixed = BTreeSet::from(["countries".to_string()]);
        let plan = explain_statement(
            &connection,
            "select code,(select id from participants where id=@id) from countries order by code",
            &fixed,
        )
        .unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
        let plan = explain_statement(
            &connection,
            "select name from participants where org_id=@org_id order by name",
            &fixed,
        )
        .unwrap();
        assert_eq!(plan.temporary_sorts.len(), 1, "{plan:?}");
        assert!(plan.growing_scans.is_empty(), "{plan:?}");
    }

    #[test]
    fn query_plans_follow_growing_in_lists_that_drive_point_lookups() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key, name text); create table registrations(participant_id integer);").unwrap();
        let plan = explain_statement(
            &connection,
            "select name from participants where id in (select participant_id from registrations) order by name",
            &BTreeSet::new(),
        ).unwrap();
        assert_eq!(plan.temporary_sorts.len(), 1, "{plan:?}");
    }

    #[test]
    fn query_plans_flag_growing_union_trees_but_allow_fixed_unions() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key, name text); create index participant_name on participants(name); create table countries(code text);").unwrap();
        let fixed = BTreeSet::from(["countries".to_string()]);
        let plan = explain_statement(&connection,
            "select id from participants where name=@name union select id from participants where id=@id", &fixed).unwrap();
        assert_eq!(plan.temporary_sorts.len(), 1, "{plan:?}");
        let plan = explain_statement(
            &connection,
            "select code from countries union select 'CA'",
            &fixed,
        )
        .unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
    }

    #[test]
    fn query_plans_follow_materialized_inputs_without_treating_the_cte_as_an_unknown_table() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key, org_id integer, name text); create index participant_org on participants(org_id);").unwrap();
        let plan=explain_statement(&connection,"with selected as materialized (select name from participants where org_id=@org_id) select name from selected order by name",&BTreeSet::new()).unwrap();
        assert_eq!(plan.temporary_sorts.len(), 1, "{plan:?}");
        assert!(plan.unresolved_reads.is_empty(), "{plan:?}");
    }
    #[test]
    fn query_plans_distinguish_fts_search_from_an_unconstrained_virtual_scan() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("create virtual table participant_names using fts5(name);")
            .unwrap();
        let scan = explain_statement(
            &connection,
            "select n.rowid from participant_names n",
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            scan.growing_scans,
            BTreeSet::from(["participant_names".to_string()])
        );
        let search = explain_statement(
            &connection,
            "select rowid from participant_names where participant_names match @name",
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(search.growing_scans.is_empty(), "{search:?}");
        assert!(search.unresolved_reads.is_empty(), "{search:?}");
    }

    #[test]
    fn query_plans_resolve_view_aliases_and_never_execute_mutations() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key,name text); insert into participants values(1,'Test'); create view visible as select p.id,p.name from participants p;").unwrap();
        let plan =
            explain_statement(&connection, "select id from visible", &BTreeSet::new()).unwrap();
        assert_eq!(
            plan.growing_scans,
            BTreeSet::from(["participants".to_string()])
        );
        let plan = explain_statement(
            &connection,
            "delete from participants where id=1",
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(plan.growing_scans.is_empty(), "{plan:?}");
        assert_eq!(
            connection
                .query_row("select count(*) from participants", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn query_plans_resolve_left_join_sources_and_json_iteration() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key,org_id integer,name text); create table questions(org_id integer,key text);").unwrap();
        let plan=explain_statement(&connection,"select ids.value,q.key from json_each(@ids) ids left join questions q on q.org_id=ids.value",&BTreeSet::new()).unwrap();
        assert!(plan.unresolved_reads.is_empty(), "{plan:?}");
        assert!(plan.growing_scans.contains("questions"), "{plan:?}");
    }

    #[test]
    fn query_plans_do_not_flag_sorting_a_materialized_point_lookup() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("create table participants(id integer primary key,name text);")
            .unwrap();
        let plan=explain_statement(&connection,"with selected as materialized (select name from participants where id=@id) select name from selected order by name",&BTreeSet::new()).unwrap();
        assert!(plan.temporary_sorts.is_empty(), "{plan:?}");
        assert!(plan.growing_scans.is_empty(), "{plan:?}");
    }
    #[test]
    fn query_plans_resolve_compound_ctes_and_reused_physical_aliases() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key,name text); create table orders(id integer primary key,name text);").unwrap();
        let plan=explain_statement(&connection,"with assignments as (select id from participants union all select id from orders) select a.id from assignments a join participants p on p.id=a.id",&BTreeSet::new()).unwrap();
        assert!(plan.unresolved_reads.is_empty(), "{plan:?}");
        let plan = explain_statement(
            &connection,
            "select s.id from participants s union all select s.id from orders s",
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(plan.unresolved_reads.is_empty(), "{plan:?}");
        assert!(!plan.growing_scans.is_empty(), "{plan:?}");
    }
    #[test]
    fn query_plans_resolve_indexed_exists_search_inputs() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key); create table marks(participant_id integer, seq integer, primary key(participant_id,seq)) without rowid;").unwrap();
        let plan=explain_statement(&connection,"select p.id from participants p where exists(select 1 from marks m where m.participant_id=p.id and m.seq>0)",&BTreeSet::new()).unwrap();
        assert!(plan.unresolved_reads.is_empty(), "{plan:?}");
        assert_eq!(
            plan.growing_scans,
            BTreeSet::from(["participants".to_string()])
        );
    }
    #[test]
    fn query_plans_preserve_quoted_aliases_ending_in_exists() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("create table participants(id integer primary key);")
            .unwrap();
        let plan = explain_statement(
            &connection,
            "select \"m EXISTS\".id from participants as \"m EXISTS\" where \"m EXISTS\".id=@id",
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(plan.unresolved_reads.is_empty(), "{plan:?}");
        assert!(plan.growing_scans.is_empty(), "{plan:?}");
    }
}
