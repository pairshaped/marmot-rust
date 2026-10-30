//! Inspect raw SQL without executing its application statement.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use serde::Serialize;

use crate::sqlite::{
    parse,
    tokenize::{Token, tokenize},
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
}

#[derive(Debug)]
struct Catalog {
    tables: BTreeSet<String>,
    fts5: BTreeSet<String>,
    indexes: BTreeMap<String, String>,
    views: BTreeMap<String, String>,
}

impl Catalog {
    fn load(connection: &Connection) -> rusqlite::Result<Self> {
        let mut catalog = Self {
            tables: BTreeSet::new(),
            fts5: BTreeSet::new(),
            indexes: BTreeMap::new(),
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
        Ok(catalog)
    }

    fn aliases(&self, sql: &str) -> (BTreeMap<String, BTreeSet<String>>, String) {
        let mut aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut pending = vec![sql.to_owned()];
        let mut visited = BTreeSet::new();
        while let Some(sql) = pending.pop() {
            let tokens = tokenize(&sql);
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
                let table = binding.table.name;
                aliases
                    .entry(binding.alias.unwrap_or_else(|| table.clone()))
                    .or_default()
                    .insert(table.clone());
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
        (aliases, hash)
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
        let source = read_source(detail)?.to_ascii_lowercase();

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

fn read_source(detail: &str) -> Option<&str> {
    let detail = detail.strip_suffix(" LEFT-JOIN").unwrap_or(detail);
    let source = detail
        .strip_prefix("SCAN ")
        .or_else(|| detail.strip_prefix("SEARCH "))?;
    let source = source.strip_prefix("TABLE ").unwrap_or(source);
    Some(
        source
            .split(" USING ")
            .next()
            .unwrap()
            .split(" VIRTUAL TABLE ")
            .next()
            .unwrap()
            .trim(),
    )
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
        let has_point_lookup = self.steps.iter().any(|step| {
            step.parent == parent && matches!(self.reads.get(&step.id), Some(ReadInput::Point))
        });
        for step in self
            .steps
            .iter()
            .filter(|step| step.parent == parent && step.id != parent)
        {
            if self.reads.contains_key(&step.id) {
                tables.extend(self.tables(step.id, visited));
            } else if !step.detail.contains("SCALAR SUBQUERY")
                && (!step.detail.contains("LIST SUBQUERY") || has_point_lookup)
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
    let sql = crate::analyzer::strip_nullability_overrides(sql);
    let mut statement = connection.prepare(&format!("explain query plan {sql}"))?;
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
    let catalog = Catalog::load(connection)?;
    let (aliases, source_hash) = catalog.aliases(&sql);
    let parents = steps
        .iter()
        .map(|step| (step.id, step.parent))
        .collect::<BTreeMap<_, _>>();
    let definition_for = |source: &str, step: &PlanStep| {
        let mut names = aliases.get(source).cloned().unwrap_or_default();
        names.insert(source.to_owned());
        let mut scope = step.parent;
        let mut visited = BTreeSet::new();
        while visited.insert(scope) {
            let candidates = steps
                .iter()
                .filter(|definition| {
                    definition.parent == scope
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
                materialized_name(&definition.detail).is_some_and(|name| names.contains(name))
            })
            .map(|definition| definition.id)
            .collect::<Vec<_>>();
        (!shared.is_empty()).then_some(shared)
    };
    let mut unresolved_reads = BTreeSet::new();
    let mut reads = BTreeMap::new();
    for step in &steps {
        let Some(source) = read_source(&step.detail) else {
            continue;
        };
        let input = if step.detail.starts_with("SEARCH ")
            && step.detail.contains("USING INTEGER PRIMARY KEY (rowid=?)")
        {
            ReadInput::Point
        } else if let Some(table) = catalog.read_table(&step.detail, &aliases) {
            ReadInput::Table(table)
        } else if let Some(parent) = definition_for(source, step) {
            ReadInput::Materialized(parent)
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
            _ => read_source(&step.detail).map(str::to_owned),
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

    Ok(StatementPlan {
        source_hash,
        finding_counts,
        steps,
        growing_scans,
        temporary_sorts,
        unresolved_reads,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

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
}
