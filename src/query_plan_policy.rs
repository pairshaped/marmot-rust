//! Audit companion SQL plans with explicit, source-bound exceptions.
use crate::query_plans::{PlanInspector, StatementPlan, digest, normalized_tokens};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// Parent positions, rather than SQLite instruction IDs, keep physical topology
// stable when SQLite inserts a foreign-key child lookup.
pub type PlanShapes = Vec<(Option<String>, Vec<(Option<usize>, String)>)>;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineEntry {
    pub statement: String,
    pub fingerprint: String,
    pub plan_hash: String,
    pub findings: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_shapes: Option<PlanShapes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub foreign_key_searches: BTreeSet<String>,
}

impl BaselineEntry {
    fn permits(&self, current: &Self) -> bool {
        if self.statement != current.statement {
            return false;
        }
        if self.fingerprint == current.fingerprint
            && self.plan_hash == current.plan_hash
            && self.findings == current.findings
        {
            return true;
        }
        if self.findings == current.findings
            && self.sql_fingerprint.is_some()
            && self.sql_fingerprint == current.sql_fingerprint
            && let (Some(before), Some(after)) = (&self.plan_shapes, &current.plan_shapes)
            && added_indexed_searches_only(before, after, &current.foreign_key_searches)
        {
            return true;
        }
        let no_new = current
            .findings
            .iter()
            .all(|(finding, count)| self.findings.get(finding).is_some_and(|old| count <= old));
        no_new && current.findings.values().sum::<usize>() < self.findings.values().sum::<usize>()
    }
}

type ForeignKeyIndexes = BTreeMap<String, BTreeSet<(String, String)>>;

fn foreign_key_indexes(connection: &rusqlite::Connection) -> rusqlite::Result<ForeignKeyIndexes> {
    let mut query = connection.prepare(
        "select distinct lower(fk.\"table\"), child.name, idx.name
         from sqlite_schema child, pragma_foreign_key_list(child.name) fk, pragma_index_list(child.name) idx
         where child.type='table' and idx.partial=0 and not exists (
           select 1 from pragma_foreign_key_list(child.name) part
           where part.id=fk.id and not exists (
             select 1 from pragma_index_info(idx.name) col
             where col.seqno=part.seq and col.name=part.\"from\" collate nocase
           )
         )")?;
    let mut indexes = ForeignKeyIndexes::new();
    for row in query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (parent, child, index) = row?;
        indexes.entry(parent).or_default().insert((child, index));
    }
    Ok(indexes)
}

fn foreign_key_searches(
    sql: &str,
    variants: &[Variant],
    indexes: &ForeignKeyIndexes,
) -> BTreeSet<String> {
    use crate::sqlite::{
        parse::{Statement, parse_statement},
        tokenize::tokenize,
    };
    let parent = match parse_statement(&tokenize(sql)) {
        Statement::Insert(statement) => statement.target.table,
        Statement::Update(statement) => statement.target.table,
        Statement::Delete(statement) => statement.target.table,
        _ => return BTreeSet::new(),
    };
    if parent
        .schema
        .as_deref()
        .is_some_and(|schema| !schema.eq_ignore_ascii_case("main"))
    {
        return BTreeSet::new();
    }
    let Some(children) = indexes.get(&parent.name.to_lowercase()) else {
        return BTreeSet::new();
    };
    variants
        .iter()
        .flat_map(|variant| &variant.plan.steps)
        .filter(|step| {
            children.iter().any(|(child, index)| {
                ["INDEX", "COVERING INDEX"].iter().any(|kind| {
                    step.detail
                        .starts_with(&format!("SEARCH {child} USING {kind} {index} ("))
                })
            })
        })
        .map(|step| step.detail.clone())
        .collect()
}

fn plan_shapes(variants: &[Variant]) -> PlanShapes {
    variants
        .iter()
        .map(|variant| {
            let positions = variant
                .plan
                .steps
                .iter()
                .enumerate()
                .map(|(position, step)| (step.id, position))
                .collect::<BTreeMap<_, _>>();
            (
                variant.column.clone(),
                variant
                    .plan
                    .steps
                    .iter()
                    .map(|step| (positions.get(&step.parent).copied(), step.detail.clone()))
                    .collect(),
            )
        })
        .collect()
}

fn added_indexed_searches_only(
    before: &PlanShapes,
    after: &PlanShapes,
    foreign_key_searches: &BTreeSet<String>,
) -> bool {
    if before.len() != after.len() {
        return false;
    }
    let mut added = false;
    for ((old_column, old), (new_column, new)) in before.iter().zip(after) {
        if old_column != new_column {
            return false;
        }
        let mut matched = 0;
        let mut positions = BTreeMap::new();
        for (position, (parent, detail)) in new.iter().enumerate() {
            let mapped_parent = match parent {
                None => None,
                Some(parent) => match positions.get(parent) {
                    Some(parent) => Some(*parent),
                    None => return false,
                },
            };
            if old
                .get(matched)
                .is_some_and(|step| step == &(mapped_parent, detail.clone()))
            {
                positions.insert(position, matched);
                matched += 1;
            } else {
                // Additional work must be an indexed leaf lookup. A new scan,
                // sort, primary-key probe or subtree still needs review.
                let indexed = detail.starts_with("SEARCH ")
                    && (detail.contains(" USING INDEX ")
                        || detail.contains(" USING COVERING INDEX "));
                if !indexed
                    || !foreign_key_searches.contains(detail)
                    || new.iter().any(|(parent, _)| *parent == Some(position))
                {
                    return false;
                }
                added = true;
            }
        }
        if matched != old.len() {
            return false;
        }
    }
    added
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub version: u32,
    pub entries: Vec<BaselineEntry>,
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    pub fixed_tables: BTreeMap<String, String>,
    pub exceptions: Vec<Exception>,
    #[serde(skip)]
    pub baseline: Baseline,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    pub statement: String,
    pub source_hash: String,
    pub plan_hash: String,
    pub owner: String,
    pub reason: String,
    pub follow_up: String,
    pub findings: Vec<String>,
}

impl Policy {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("expected policy version 1".into());
        }
        for (table, reason) in &self.fixed_tables {
            if table.trim().is_empty() || reason.trim().is_empty() {
                return Err("fixed tables require a name and reason".into());
            }
        }
        let mut statements = BTreeSet::new();
        for exception in &self.exceptions {
            if !statements.insert(&exception.statement) {
                return Err(format!("duplicate exception for {}", exception.statement));
            }
            if [
                &exception.statement,
                &exception.owner,
                &exception.reason,
                &exception.follow_up,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
                || exception.findings.is_empty()
            {
                return Err(
                    "exceptions require statement, owner, reason, follow_up and findings".into(),
                );
            }
            for hash in [&exception.source_hash, &exception.plan_hash] {
                if hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(
                        "exceptions require lowercase SHA-256 source and plan hashes".into(),
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct Variant {
    pub column: Option<String>,
    pub plan: StatementPlan,
}

#[derive(Debug, Serialize)]
pub struct StatementReport {
    pub statement: String,
    pub source_hash: String,
    pub sql_fingerprint: String,
    pub foreign_key_searches: BTreeSet<String>,
    pub plan_hash: String,
    pub variants: Vec<Variant>,
    pub findings: BTreeSet<String>,
    pub finding_counts: BTreeMap<String, usize>,
    pub excepted: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_fingerprint: String,
    pub sqlite_version: String,
    pub statements: Vec<StatementReport>,
    pub unused_exceptions: Vec<String>,
    pub next_baseline: Baseline,
}

impl StatementReport {
    pub fn baseline_entry(&self) -> BaselineEntry {
        BaselineEntry {
            statement: self.statement.clone(),
            fingerprint: self.source_hash.clone(),
            plan_hash: self.plan_hash.clone(),
            findings: self.finding_counts.clone(),
            plan_shapes: Some(plan_shapes(&self.variants)),
            sql_fingerprint: Some(self.sql_fingerprint.clone()),
            foreign_key_searches: self.foreign_key_searches.clone(),
        }
    }
}

impl Baseline {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("expected baseline version 1".into());
        }
        let mut seen = BTreeSet::new();
        for entry in &self.entries {
            if entry.sql_fingerprint.as_ref().is_some_and(|hash| {
                hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }) {
                return Err("baseline SQL fingerprint must be SHA-256".into());
            }
            if !entry.foreign_key_searches.is_empty()
                && (entry.sql_fingerprint.is_none()
                    || !entry.plan_shapes.as_ref().is_some_and(|shapes| {
                        entry.foreign_key_searches.iter().all(|search| {
                            shapes
                                .iter()
                                .any(|(_, steps)| steps.iter().any(|(_, detail)| detail == search))
                        })
                    }))
            {
                return Err(
                    "baseline FK searches require SQL identity and matching physical steps".into(),
                );
            }
            if let Some(shapes) = &entry.plan_shapes
                && digest(&serde_json::to_vec(shapes).expect("serialize query plans"))
                    != entry.plan_hash
            {
                return Err("baseline physical plan does not match its hash".into());
            }
            for hash in [&entry.fingerprint, &entry.plan_hash] {
                if hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err("baseline requires SHA-256 source and plan fingerprints".into());
                }
            }
            if !seen.insert(&entry.statement)
                || entry.findings.is_empty()
                || entry.findings.values().any(|count| *count == 0)
                || entry
                    .findings
                    .keys()
                    .any(|finding| finding.starts_with("unresolved:"))
            {
                return Err(
                    "baseline entries must be unique statements with resolved, nonzero findings"
                        .into(),
                );
            }
        }
        Ok(())
    }
    pub fn is_reduction_of(&self, previous: &Self) -> bool {
        if self.validate().is_err() || previous.validate().is_err() {
            return false;
        }
        let old = previous
            .entries
            .iter()
            .map(|entry| (&entry.statement, entry))
            .collect::<BTreeMap<_, _>>();
        self.entries.iter().all(|entry| {
            old.get(&entry.statement)
                .is_some_and(|previous| previous.permits(entry))
        })
    }
}

impl Report {
    pub fn passed(&self) -> bool {
        self.unused_exceptions.is_empty()
            && self
                .statements
                .iter()
                .all(|statement| statement.failure.is_none())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid query-plan policy: {0}")]
    Policy(String),
    #[error(transparent)]
    Source(#[from] crate::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub fn write_baseline(path: &Path, baseline: &Baseline) -> Result<(), Error> {
    use std::io::Write;
    baseline.validate().map_err(Error::Policy)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| crate::Error::WriteFile {
            path: path.to_owned(),
            source,
        })?;
    let mut bytes = serde_json::to_vec_pretty(baseline).expect("serialize baseline");
    bytes.push(b'\n');
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| crate::Error::WriteFile {
            path: path.to_owned(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| crate::Error::WriteFile {
            path: path.to_owned(),
            source: error.error,
        })?;
    Ok(())
}

pub fn check_schema(
    schema: &Path,
    source_roots: &[PathBuf],
    init_sql: Option<&Path>,
    label_root: &Path,
    policy: &Policy,
) -> Result<Report, Error> {
    let connection = rusqlite::Connection::open_in_memory()?;
    connection.pragma_update(None, "foreign_keys", true)?;
    // Plans need function names/flags, not the type-analysis TEMP view shadows.
    if let Some(path) = init_sql {
        let sql = std::fs::read_to_string(path).map_err(|source| crate::Error::ReadInitSql {
            path: path.to_path_buf(),
            source,
        })?;
        crate::analyzer::register_analysis_functions(&connection, path, &sql)?;
    }
    let sql = std::fs::read_to_string(schema).map_err(|source| crate::Error::ReadFile {
        path: schema.to_path_buf(),
        source,
    })?;
    connection.execute_batch(&sql)?;
    check_sources(&connection, source_roots, label_root, policy)
}

pub fn check_sources(
    connection: &rusqlite::Connection,
    source_roots: &[PathBuf],
    label_root: &Path,
    policy: &Policy,
) -> Result<Report, Error> {
    policy.validate().map_err(Error::Policy)?;
    policy.baseline.validate().map_err(Error::Policy)?;
    let old_baseline = policy
        .baseline
        .entries
        .iter()
        .map(|entry| (entry.statement.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut next_entries = Vec::new();
    for table in policy.fixed_tables.keys() {
        let exists: bool = connection.query_row(
            "select exists(select 1 from sqlite_schema where type='table' and name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(Error::Policy(format!("fixed table {table} does not exist")));
        }
    }
    let fixed = policy.fixed_tables.keys().cloned().collect::<BTreeSet<_>>();
    let inspector = PlanInspector::new(connection)?;
    let foreign_key_indexes = foreign_key_indexes(connection)?;
    let exceptions = policy
        .exceptions
        .iter()
        .map(|exception| (exception.statement.as_str(), exception))
        .collect::<BTreeMap<_, _>>();
    let mut statements = Vec::new();
    let mut seen = BTreeSet::new();
    let mut seen_files = BTreeSet::new();
    for root in source_roots {
        for file in crate::discover_sql_files(root)? {
            if !seen_files.insert(file.path.clone()) {
                continue;
            }
            let source =
                std::fs::read_to_string(&file.path).map_err(|source| crate::Error::ReadFile {
                    path: file.path.clone(),
                    source,
                })?;
            let blocks = crate::sqlite::blocks::parse_sql_blocks(&source).map_err(|reason| {
                crate::Error::InvalidSqlBlock {
                    path: file.path.clone(),
                    reason,
                }
            })?;
            for block in blocks {
                let label = format!(
                    "{}#{}",
                    file.path
                        .strip_prefix(label_root)
                        .unwrap_or(&file.path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    block.function_name
                );
                seen.insert(label.clone());
                let choices = block
                    .column_substitution
                    .as_ref()
                    .map(|columns| {
                        columns
                            .choices
                            .iter()
                            .map(|column| Some(column.clone()))
                            .collect()
                    })
                    .unwrap_or_else(|| vec![None]);
                let mut variants = Vec::new();
                let mut findings = BTreeSet::new();
                let mut finding_counts: BTreeMap<String, usize> = BTreeMap::new();
                for column in choices {
                    let sql = column
                        .as_ref()
                        .map(|column| block.sql.replace("{{column}}", &format!("\"{column}\"")))
                        .unwrap_or_else(|| block.sql.clone());
                    let sql = crate::sql_text::validate_sql(&sql).map_err(|reason| {
                        crate::Error::InvalidSql {
                            path: file.path.clone(),
                            reason,
                        }
                    })?;
                    let plan = inspector.explain(&sql, &fixed).map_err(|source| {
                        crate::Error::PrepareSql {
                            path: PathBuf::from(&label),
                            source,
                        }
                    })?;
                    for (finding, count) in &plan.finding_counts {
                        let key = match &column {
                            Some(column) => format!("column:{column}:{finding}"),
                            None => finding.clone(),
                        };
                        finding_counts.insert(key, *count);
                    }
                    findings.extend(
                        plan.growing_scans
                            .iter()
                            .map(|table| format!("scan:{table}")),
                    );
                    findings.extend(
                        plan.temporary_sorts
                            .iter()
                            .map(|sort| format!("sort:{sort}")),
                    );
                    findings.extend(
                        plan.unresolved_reads
                            .iter()
                            .map(|read| format!("unresolved:{read}")),
                    );
                    variants.push(Variant { column, plan });
                }
                let hashes = variants
                    .iter()
                    .map(|variant| (&variant.column, &variant.plan.source_hash))
                    .collect::<Vec<_>>();
                let source_hash = digest(
                    &serde_json::to_vec(&(
                        &label,
                        normalized_tokens(&crate::analyzer::strip_nullability_overrides(
                            &block.sql,
                        )),
                        hashes,
                    ))
                    .expect("serialize source hash"),
                );
                let sql_fingerprint = digest(
                    &serde_json::to_vec(&normalized_tokens(
                        &crate::analyzer::strip_nullability_overrides(&block.sql),
                    ))
                    .expect("serialize SQL"),
                );
                let foreign_key_searches =
                    foreign_key_searches(&block.sql, &variants, &foreign_key_indexes);
                let shapes = plan_shapes(&variants);
                let plan_hash =
                    digest(&serde_json::to_vec(&shapes).expect("serialize query plans"));
                let current = BaselineEntry {
                    statement: label.clone(),
                    fingerprint: source_hash.clone(),
                    plan_hash: plan_hash.clone(),
                    findings: finding_counts.clone(),
                    plan_shapes: Some(shapes),
                    sql_fingerprint: Some(sql_fingerprint.clone()),
                    foreign_key_searches: foreign_key_searches.clone(),
                };
                let mut excepted = false;
                let failure = if let Some(exception) = exceptions.get(label.as_str()) {
                    if findings.is_empty() {
                        Some("remove stale exception for a clean statement".into())
                    } else if exception.source_hash != source_hash {
                        Some("exception source changed; review it".into())
                    } else if exception.plan_hash != plan_hash {
                        Some("exception plan changed; review it".into())
                    } else if exception.findings.iter().cloned().collect::<BTreeSet<_>>()
                        != findings
                    {
                        Some("exception findings changed; review it".into())
                    } else {
                        excepted = true;
                        None
                    }
                } else if findings.is_empty() {
                    None
                } else if findings
                    .iter()
                    .any(|finding| finding.starts_with("unresolved:"))
                {
                    Some("unresolved plan input; fix the resolver before baselining".into())
                } else if old_baseline
                    .get(label.as_str())
                    .is_some_and(|previous| previous.permits(&current))
                {
                    next_entries.push(current);
                    None
                } else {
                    Some("new or worse query-plan findings".into())
                };
                statements.push(StatementReport {
                    statement: label,
                    source_hash,
                    sql_fingerprint,
                    foreign_key_searches,
                    plan_hash,
                    variants,
                    findings,
                    finding_counts,
                    excepted,
                    failure,
                });
            }
        }
    }
    statements.sort_by(|left, right| left.statement.cmp(&right.statement));
    next_entries.sort_by(|left, right| left.statement.cmp(&right.statement));
    let unused_exceptions = exceptions
        .keys()
        .filter(|statement| !seen.contains(**statement))
        .map(|statement| statement.to_string())
        .collect();
    Ok(Report {
        schema_fingerprint: crate::schema::fingerprint(connection)?,
        sqlite_version: rusqlite::version().to_owned(),
        statements,
        unused_exceptions,
        next_baseline: Baseline {
            version: 1,
            entries: next_entries,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_plan_policy_requires_a_reviewable_exception() {
        let policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            baseline: Baseline::default(),
            exceptions: vec![Exception {
                statement: "src/people.sql#list".into(),
                source_hash: "0".repeat(64),
                plan_hash: "0".repeat(64),
                owner: "".into(),
                reason: "".into(),
                follow_up: "".into(),
                findings: vec!["scan:participants".into()],
            }],
        };
        assert!(policy.validate().is_err());
    }
    fn fixture(sql: &str) -> (tempfile::TempDir, rusqlite::Connection, Vec<PathBuf>) {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("src");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("people.rs"), "").unwrap();
        std::fs::write(source.join("people.sql"), sql).unwrap();
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch("create table participants(id integer primary key,name text,org_id integer); insert into participants values(1,'Test',1);").unwrap();
        (directory, connection, vec![source])
    }

    #[test]
    fn query_plan_policy_binds_exceptions_to_the_actual_sql_and_plan() {
        let (directory, connection, roots) =
            fixture("-- func: list\nselect p.id from participants p where name=@name");
        let mut policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            baseline: Baseline::default(),
            exceptions: Vec::new(),
        };
        let report = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        assert!(!report.passed());
        let row = &report.statements[0];
        policy.exceptions.push(Exception {
            statement: row.statement.clone(),
            source_hash: row.source_hash.clone(),
            plan_hash: row.plan_hash.clone(),
            owner: "SQL Optimization".into(),
            reason: "Existing name scan needs an index".into(),
            follow_up: "sports-21ba".into(),
            findings: row.findings.iter().cloned().collect(),
        });
        assert!(
            check_sources(&connection, &roots, directory.path(), &policy)
                .unwrap()
                .passed()
        );
        std::fs::write(
            roots[0].join("people.sql"),
            "-- func: list\nselect p.id from participants p where name=@name and org_id=@org_id",
        )
        .unwrap();
        assert!(
            !check_sources(&connection, &roots, directory.path(), &policy)
                .unwrap()
                .passed()
        );
        std::fs::write(
            roots[0].join("people.sql"),
            "-- func: list\nselect p.id from participants p where id=@id",
        )
        .unwrap();
        assert!(
            !check_sources(&connection, &roots, directory.path(), &policy)
                .unwrap()
                .passed()
        );
    }

    #[test]
    fn query_plan_policy_checks_every_column_variant_without_generated_bindings() {
        let (directory, connection, roots) = fixture(
            "-- func: update\n-- columns: name, org_id\nupdate participants set {{column}}=@value where name=@name",
        );
        let policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            baseline: Baseline::default(),
            exceptions: Vec::new(),
        };
        let report = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        assert_eq!(report.statements[0].variants.len(), 2);
        assert!(!report.passed());
        assert_eq!(
            connection
                .query_row("select name from participants where id=1", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            "Test"
        );
    }

    #[test]
    fn query_plan_policy_fingerprints_referenced_views() {
        let (directory, connection, roots) = fixture("-- func: list\nselect id from visible");
        connection
            .execute_batch("create view visible as select id,name from participants")
            .unwrap();
        let policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            baseline: Baseline::default(),
            exceptions: Vec::new(),
        };
        let before = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        connection.execute_batch("drop view visible; create view visible as select id,name from participants where name is not null").unwrap();
        let after = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        assert_ne!(
            before.statements[0].source_hash,
            after.statements[0].source_hash
        );
    }
    #[test]
    fn query_plan_baseline_accepts_only_the_same_or_smaller_finding_budget() {
        let previous = BaselineEntry {
            statement: "src/people.sql#list".into(),
            fingerprint: "old-source".into(),
            plan_hash: "old-plan".into(),
            findings: BTreeMap::from([("scan:participants".into(), 2)]),
            plan_shapes: None,
            sql_fingerprint: None,
            foreign_key_searches: BTreeSet::new(),
        };
        assert!(previous.permits(&previous));
        let mut improved = previous.clone();
        improved.fingerprint = "new-source".into();
        improved.plan_hash = "new-plan".into();
        improved.findings.insert("scan:participants".into(), 1);
        assert!(previous.permits(&improved));
        let mut worse = previous.clone();
        worse.findings.insert("scan:participants".into(), 3);
        assert!(!previous.permits(&worse));
        let mut changed = previous.clone();
        changed.plan_hash = "unreviewed-plan".into();
        assert!(!previous.permits(&changed));
        improved.findings.insert("sort:new".into(), 1);
        assert!(!previous.permits(&improved));
    }
    #[test]
    fn query_plan_baseline_ratchets_source_fixes_and_rejects_duplicate_scan_regressions() {
        let (directory, connection, roots) =
            fixture("-- func: list\nselect id from participants where name=@name");
        let mut policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            exceptions: Vec::new(),
            baseline: Baseline::default(),
        };
        let first = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        policy
            .baseline
            .entries
            .push(first.statements[0].baseline_entry());
        assert!(
            check_sources(&connection, &roots, directory.path(), &policy)
                .unwrap()
                .passed()
        );
        std::fs::write(roots[0].join("people.sql"),"-- func: list\nselect id from participants where name=@name union all select id from participants where name=@name").unwrap();
        assert!(
            !check_sources(&connection, &roots, directory.path(), &policy)
                .unwrap()
                .passed()
        );
        std::fs::write(
            roots[0].join("people.sql"),
            "-- func: list\nselect id from participants where id=@id",
        )
        .unwrap();
        let fixed = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        assert!(fixed.passed());
        assert!(fixed.next_baseline.entries.is_empty());
        assert!(fixed.next_baseline.is_reduction_of(&policy.baseline));
        assert!(!policy.baseline.is_reduction_of(&fixed.next_baseline));
    }
    #[test]
    fn query_plan_baseline_rejects_a_worse_scan_even_if_a_sort_disappears() {
        let (directory, connection, roots) = fixture(
            "-- func: list\nselect name from participants indexed by participant_name order by id",
        );
        connection
            .execute_batch("create index participant_name on participants(name)")
            .unwrap();
        let mut policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            exceptions: Vec::new(),
            baseline: Baseline::default(),
        };
        let old = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        policy
            .baseline
            .entries
            .push(old.statements[0].baseline_entry());
        std::fs::write(
            roots[0].join("people.sql"),
            "-- func: list\nselect name from participants not indexed",
        )
        .unwrap();
        let worse = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        assert!(
            !worse.passed(),
            "dropping an unrelated sort cannot excuse a worse scan: {worse:?}"
        );
    }
    #[test]
    fn query_plan_baseline_accepts_an_added_indexed_foreign_key_check() {
        let (directory, connection, roots) =
            fixture("-- func: remove\ndelete from participants where id=@id");
        connection.execute_batch("pragma foreign_keys=on; create table retained_child(participant_id integer references participants(id));").unwrap();
        let mut policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            exceptions: Vec::new(),
            baseline: Baseline::default(),
        };
        let before = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        policy
            .baseline
            .entries
            .push(before.statements[0].baseline_entry());
        connection.execute_batch("create table indexed_child(participant_id integer references participants(id)); create index indexed_child_parent on indexed_child(participant_id);").unwrap();
        let after = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        assert_eq!(
            before.statements[0].finding_counts,
            after.statements[0].finding_counts
        );
        assert_ne!(
            before.statements[0].plan_hash,
            after.statements[0].plan_hash
        );
        assert!(
            after.passed(),
            "an added indexed FK lookup preserves existing findings: {after:?}"
        );
        assert!(after.next_baseline.is_reduction_of(&policy.baseline));
    }
    #[test]
    fn query_plan_baseline_limits_additions_to_indexed_leaf_searches() {
        let old_shapes = vec![(None, vec![(None, "SCAN retained".to_string())])];
        let old = BaselineEntry {
            statement: "src/people.sql#list".into(),
            fingerprint: "0".repeat(64),
            plan_hash: digest(&serde_json::to_vec(&old_shapes).unwrap()),
            findings: BTreeMap::from([("scan:retained".into(), 1)]),
            plan_shapes: Some(old_shapes.clone()),
            sql_fingerprint: Some("0".repeat(64)),
            foreign_key_searches: BTreeSet::new(),
        };
        let baseline = Baseline {
            version: 1,
            entries: vec![old.clone()],
        };
        for detail in [
            "SEARCH child USING INDEX child_parent (parent_id=?)",
            "SEARCH child USING COVERING INDEX child_parent (parent_id=?)",
        ] {
            let shapes = vec![(
                None,
                vec![(None, detail.to_string()), (None, "SCAN retained".into())],
            )];
            let entry = BaselineEntry {
                plan_hash: digest(&serde_json::to_vec(&shapes).unwrap()),
                plan_shapes: Some(shapes),
                foreign_key_searches: BTreeSet::from([detail.to_string()]),
                ..old.clone()
            };
            assert!(
                Baseline {
                    version: 1,
                    entries: vec![entry]
                }
                .is_reduction_of(&baseline),
                "{detail}"
            );
        }
        for steps in [
            vec![
                (None, "SCAN child".to_string()),
                (None, "SCAN retained".into()),
            ],
            vec![
                (None, "USE TEMP B-TREE FOR ORDER BY".into()),
                (None, "SCAN retained".into()),
            ],
            vec![
                (
                    None,
                    "SEARCH child USING INTEGER PRIMARY KEY (rowid=?)".into(),
                ),
                (None, "SCAN retained".into()),
            ],
            vec![
                (
                    None,
                    "SEARCH child USING INDEX child_parent (parent_id=?)".into(),
                ),
                (Some(0), "SCAN retained".into()),
            ],
            vec![(
                None,
                "SEARCH child USING INDEX child_parent (parent_id=?)".into(),
            )],
            vec![(None, "SCAN changed".into())],
        ] {
            let shapes = vec![(None, steps)];
            let entry = BaselineEntry {
                plan_hash: digest(&serde_json::to_vec(&shapes).unwrap()),
                plan_shapes: Some(shapes),
                ..old.clone()
            };
            assert!(
                !Baseline {
                    version: 1,
                    entries: vec![entry]
                }
                .is_reduction_of(&baseline)
            );
        }
        let mut forged = old.clone();
        forged.plan_shapes.as_mut().unwrap()[0].1.push((
            None,
            "SEARCH child USING INDEX child_parent (parent_id=?)".into(),
        ));
        assert!(
            Baseline {
                version: 1,
                entries: vec![forged]
            }
            .validate()
            .is_err()
        );
        let mut legacy = old.clone();
        legacy.plan_shapes = None;
        assert!(
            baseline.is_reduction_of(&Baseline {
                version: 1,
                entries: vec![legacy]
            }),
            "old exact hashes can acquire matching physical evidence"
        );
    }
    #[test]
    fn query_plan_baseline_rejects_a_new_indexed_read_in_changed_sql() {
        let (directory, connection, roots) =
            fixture("-- func: list\nselect p.id from participants p");
        connection.execute_batch("create table lookup(parent_id integer); create index lookup_parent on lookup(parent_id);").unwrap();
        let mut policy = Policy {
            version: 1,
            fixed_tables: BTreeMap::new(),
            exceptions: Vec::new(),
            baseline: Baseline::default(),
        };
        let before = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        policy
            .baseline
            .entries
            .push(before.statements[0].baseline_entry());
        std::fs::write(roots[0].join("people.sql"),"-- func: list\nselect p.id from participants p cross join lookup l indexed by lookup_parent where l.parent_id=p.id").unwrap();
        let after = check_sources(&connection, &roots, directory.path(), &policy).unwrap();
        assert_eq!(
            before.statements[0].finding_counts,
            after.statements[0].finding_counts
        );
        assert!(
            !after.passed(),
            "a newly written indexed read is not a mechanical FK plan change: {after:?}"
        );
    }
}
