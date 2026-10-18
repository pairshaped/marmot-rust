//! Check-mode diagnostics for adjacent bindings: staleness, owner declarations
//! and orphans. Everything here reads; nothing writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::Project;
use crate::publication::Changes;

const ADJACENT_SUFFIX: &str = ".generated.rs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// A file Marmot writes is missing, differs, or should not exist.
    Stale { path: PathBuf, state: Staleness },
    /// `owner` has no `#[path]` declaration that loads `generated`.
    MissingDeclaration {
        sql: PathBuf,
        owner: PathBuf,
        generated: PathBuf,
        conditional_in: Vec<PathBuf>,
    },
    /// More than one declaration loads `generated`.
    DuplicateDeclaration {
        generated: PathBuf,
        declared_in: Vec<PathBuf>,
    },
    /// A declaration points at a generated file that does not exist.
    DeclarationWithoutFile {
        declared_in: PathBuf,
        resolved: PathBuf,
    },
    /// A generated file whose `x.sql` is gone.
    Orphan {
        generated: PathBuf,
        declared_in: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    Missing,
    Differs,
    Unexpected,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale { path, state } => {
                let what = match state {
                    Staleness::Missing => "missing",
                    Staleness::Differs => "stale",
                    Staleness::Unexpected => "unexpected",
                };
                let fix = if *state == Staleness::Unexpected {
                    "run generate to remove it"
                } else {
                    "run generate"
                };
                write!(f, "generated file is {what}: {}; {fix}", path.display())
            }
            Self::MissingDeclaration {
                sql,
                owner,
                generated,
                conditional_in,
            } => {
                write!(
                    f,
                    "{} has no owner declaration for {}; add `#[path = \"{}\"] mod sql;` to {}",
                    sql.display(),
                    generated.display(),
                    file_name(generated),
                    owner.display(),
                )?;
                if !conditional_in.is_empty() {
                    write!(
                        f,
                        " (a declaration under #[cfg] in {} does not count)",
                        join(conditional_in)
                    )?;
                }
                Ok(())
            }
            Self::DuplicateDeclaration {
                generated,
                declared_in,
            } => write!(
                f,
                "{} is declared {} times (in {}); keep exactly one declaration",
                generated.display(),
                declared_in.len(),
                join(declared_in),
            ),
            Self::DeclarationWithoutFile {
                declared_in,
                resolved,
            } => write!(
                f,
                "{} declares {}, which does not exist; remove the declaration",
                declared_in.display(),
                resolved.display(),
            ),
            Self::Orphan {
                generated,
                declared_in,
            } => {
                write!(
                    f,
                    "orphan generated file {} has no matching .sql file; delete it",
                    generated.display()
                )?;
                if !declared_in.is_empty() {
                    write!(f, " and remove its declaration in {}", join(declared_in))?;
                }
                Ok(())
            }
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn join(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One `#[path = "…generated.rs"] mod …;` declaration.
#[derive(Debug)]
struct Declaration {
    file: PathBuf,
    resolved: PathBuf,
    conditional: bool,
}

/// Collect the problems check mode reports. Never writes.
pub(crate) fn problems(
    config: &Config,
    project: &Project,
    changes: &Changes,
    desired: &BTreeSet<PathBuf>,
) -> Result<Vec<Problem>> {
    let declarations = scan_declarations(&config.source_root)?;
    let mut by_target: BTreeMap<PathBuf, Vec<&Declaration>> = BTreeMap::new();
    for declaration in &declarations {
        by_target
            .entry(normalize(&declaration.resolved))
            .or_default()
            .push(declaration);
    }

    let mut problems = Vec::new();
    let mut claimed = BTreeSet::new();
    for query in &project.queries {
        let generated = query.source_path.with_extension("generated.rs");
        if !claimed.insert(normalize(&generated)) {
            continue;
        }
        let found = by_target
            .get(&normalize(&generated))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let live = found
            .iter()
            .filter(|declaration| !declaration.conditional)
            .map(|declaration| declaration.file.clone())
            .collect::<Vec<_>>();
        match live.len() {
            0 => problems.push(Problem::MissingDeclaration {
                sql: query.source_path.clone(),
                owner: query.source_path.with_extension("rs"),
                generated,
                conditional_in: found
                    .iter()
                    .map(|declaration| declaration.file.clone())
                    .collect(),
            }),
            1 => {}
            _ => problems.push(Problem::DuplicateDeclaration {
                generated,
                declared_in: live,
            }),
        }
    }

    for declaration in &declarations {
        let target = normalize(&declaration.resolved);
        if !claimed.contains(&target) && !declaration.resolved.exists() {
            problems.push(Problem::DeclarationWithoutFile {
                declared_in: declaration.file.clone(),
                resolved: declaration.resolved.clone(),
            });
        }
    }

    let mut orphans = BTreeSet::new();
    for path in &changes.removals {
        if is_adjacent(path) {
            orphans.insert(path.clone());
        }
    }
    for path in &orphans {
        problems.push(Problem::Orphan {
            generated: path.clone(),
            declared_in: by_target
                .get(&normalize(path))
                .map(|found| found.iter().map(|d| d.file.clone()).collect())
                .unwrap_or_default(),
        });
    }

    for path in changes.paths() {
        if orphans.contains(path) {
            continue;
        }
        let state = if !desired.contains(path) {
            Staleness::Unexpected
        } else if path.exists() {
            Staleness::Differs
        } else {
            Staleness::Missing
        };
        problems.push(Problem::Stale {
            path: path.to_path_buf(),
            state,
        });
    }
    Ok(problems)
}

fn is_adjacent(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(ADJACENT_SUFFIX))
}

fn scan_declarations(source_root: &Path) -> Result<Vec<Declaration>> {
    let mut declarations = Vec::new();
    if !source_root.exists() {
        return Ok(declarations);
    }
    for entry in walkdir::WalkDir::new(source_root).sort_by_file_name() {
        let entry = entry.map_err(|source| Error::WalkDir {
            path: source_root.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !entry.file_type().is_file()
            || path.extension().is_none_or(|extension| extension != "rs")
            || is_adjacent(path)
        {
            continue;
        }
        let text = fs::read_to_string(path).map_err(|source| Error::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        if !text.contains(ADJACENT_SUFFIX) {
            continue;
        }
        let file = syn::parse_file(&text).map_err(|error| Error::ParseRust {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
        let mut nesting = Vec::new();
        collect(&file.items, path, &mut nesting, false, &mut declarations);
    }
    Ok(declarations)
}

fn collect(
    items: &[syn::Item],
    file: &Path,
    nesting: &mut Vec<String>,
    under_cfg: bool,
    out: &mut Vec<Declaration>,
) {
    for item in items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        let attribute_path = path_attribute(&module.attrs);
        match &module.content {
            Some((_, inner)) => {
                nesting.push(attribute_path.unwrap_or_else(|| module.ident.to_string()));
                let gated = under_cfg || has_cfg(&module.attrs);
                collect(inner, file, nesting, gated, out);
                nesting.pop();
            }
            None => {
                let Some(attribute_path) = attribute_path else {
                    continue;
                };
                if !attribute_path.ends_with(ADJACENT_SUFFIX) {
                    continue;
                }
                out.push(Declaration {
                    file: file.to_path_buf(),
                    resolved: resolve(file, nesting, &attribute_path),
                    conditional: under_cfg || has_cfg(&module.attrs),
                });
            }
        }
    }
}

fn has_cfg(attributes: &[syn::Attribute]) -> bool {
    attributes
        .iter()
        .any(|attribute| attribute.path().is_ident("cfg"))
}

fn path_attribute(attributes: &[syn::Attribute]) -> Option<String> {
    attributes.iter().find_map(|attribute| {
        if !attribute.path().is_ident("path") {
            return None;
        }
        let syn::Meta::NameValue(meta) = &attribute.meta else {
            return None;
        };
        match &meta.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(value),
                ..
            }) => Some(value.value()),
            _ => None,
        }
    })
}

/// Apply Rust's `#[path]` rules. Outside inline modules the path is relative to
/// the declaring file's directory. Inside inline modules the inline names are
/// added as directories, after the file's own name when the file is not a
/// `mod.rs`, `lib.rs` or `main.rs`.
fn resolve(file: &Path, nesting: &[String], attribute_path: &str) -> PathBuf {
    let mut base = file.parent().map(Path::to_path_buf).unwrap_or_default();
    if !nesting.is_empty() {
        let mod_rs = matches!(
            file.file_name().and_then(|name| name.to_str()),
            Some("mod.rs" | "lib.rs" | "main.rs")
        );
        if !mod_rs && let Some(stem) = file.file_stem() {
            base.push(stem);
        }
        for segment in nesting {
            base.push(segment);
        }
    }
    normalize(&base.join(attribute_path))
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Target;
    use crate::emit::emit_project;
    use crate::model::{ConnectionAccess, Query, ReturnType};

    struct Fixture {
        dir: tempfile::TempDir,
        config: Config,
        project: Project,
    }

    fn write(fixture: &Fixture, relative: &str, text: &str) {
        let path = fixture.dir.path().join("src").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Three queries: `users`, `events/days` and `events/more`, generated and
    /// with no owner declarations yet.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        let queries = ["users", "events/days", "events/more"].map(|module| Query {
            source_path: root.join(format!("{module}.sql")),
            module_name: module.to_string(),
            name: "run".to_string(),
            return_type: ReturnType::Execute,
            emit_batch: false,
            scalar_cardinality: crate::model::ScalarCardinality::Many,
            connection_access: ConnectionAccess::Read,
            sql: "select 1".to_string(),
            parameters: vec![],
            column_substitution: None,
            columns: vec![],
        });
        let config = Config {
            database: dir.path().join("unused.sqlite"),
            source_root: root.clone(),
            output: root.join("generated/sql"),
            target: Target::Rust,
            check: false,
            temporal: Default::default(),
        };
        let fixture = Fixture {
            dir,
            config,
            project: Project {
                scoped_ids: vec![],
                queries: queries.to_vec(),
            },
        };
        for module in ["users", "events/days", "events/more"] {
            write(&fixture, &format!("{module}.rs"), "");
            write(&fixture, &format!("{module}.sql"), "select 1");
        }
        emit_project(&fixture.config, &fixture.project).unwrap();
        fixture
    }

    fn declare_everything(fixture: &Fixture) {
        // An alias name, a sibling declaring on its behalf, and an inline
        // module whose path is relative to the nested directory.
        write(
            fixture,
            "users.rs",
            "#[path = \"users.generated.rs\"]\nmod generated;\n",
        );
        write(
            fixture,
            "events/mod.rs",
            "#[path = \"days.generated.rs\"]\nmod days_sql;\n",
        );
        write(
            fixture,
            "events/more.rs",
            "mod tests {\n    #[path = \"../../more.generated.rs\"]\n    mod sql;\n}\n",
        );
    }

    fn check(fixture: &Fixture) -> Vec<String> {
        let mut config = fixture.config.clone();
        config.check = true;
        let root = format!("{}/src/", fixture.dir.path().display());
        match emit_project(&config, &fixture.project) {
            Ok(()) => Vec::new(),
            Err(Error::GeneratedCheckFailed { problems }) => problems
                .iter()
                .map(|problem| problem.to_string().replace(&root, ""))
                .collect(),
            Err(other) => panic!("{other}"),
        }
    }

    fn tree(fixture: &Fixture) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        walkdir::WalkDir::new(fixture.dir.path())
            .sort_by_file_name()
            .into_iter()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                    entry.metadata().unwrap().modified().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn a_clean_project_passes_with_aliases_siblings_and_inline_modules() {
        let fixture = fixture();
        declare_everything(&fixture);
        assert_eq!(check(&fixture), Vec::<String>::new());
    }

    #[test]
    fn missing_declarations_name_the_owner_and_the_declaration_to_add() {
        let fixture = fixture();
        assert_eq!(
            check(&fixture),
            [
                "users.sql has no owner declaration for users.generated.rs; add `#[path = \"users.generated.rs\"] mod sql;` to users.rs",
                "events/days.sql has no owner declaration for events/days.generated.rs; add `#[path = \"days.generated.rs\"] mod sql;` to events/days.rs",
                "events/more.sql has no owner declaration for events/more.generated.rs; add `#[path = \"more.generated.rs\"] mod sql;` to events/more.rs",
            ]
        );
    }

    #[test]
    fn two_declarations_for_one_query_are_reported() {
        let fixture = fixture();
        declare_everything(&fixture);
        write(
            &fixture,
            "events/extra.rs",
            "#[path = \"days.generated.rs\"]\nmod again;\n",
        );
        assert_eq!(
            check(&fixture),
            [
                "events/days.generated.rs is declared 2 times (in events/extra.rs, events/mod.rs); keep exactly one declaration"
            ]
        );
    }

    #[test]
    fn a_declaration_with_no_file_is_reported() {
        let fixture = fixture();
        declare_everything(&fixture);
        write(
            &fixture,
            "users.rs",
            "#[path = \"users.generated.rs\"]\nmod generated;\n#[path = \"missing.generated.rs\"]\nmod missing;\n",
        );
        assert_eq!(
            check(&fixture),
            [
                "users.rs declares missing.generated.rs, which does not exist; remove the declaration"
            ]
        );
    }

    #[test]
    fn inline_module_paths_resolve_relative_to_the_nested_directory() {
        let fixture = fixture();
        declare_everything(&fixture);
        write(
            &fixture,
            "events/more.rs",
            "mod tests {\n    #[path = \"more.generated.rs\"]\n    mod sql;\n}\n",
        );
        assert_eq!(
            check(&fixture),
            [
                "events/more.sql has no owner declaration for events/more.generated.rs; add `#[path = \"more.generated.rs\"] mod sql;` to events/more.rs",
                "events/more.rs declares events/more/tests/more.generated.rs, which does not exist; remove the declaration",
            ]
        );
    }

    #[test]
    fn an_orphan_generated_file_is_reported_with_its_declaration() {
        let fixture = fixture();
        declare_everything(&fixture);
        write(&fixture, "gone.generated.rs", "// orphan");
        write(
            &fixture,
            "gone.rs",
            "#[path = \"gone.generated.rs\"]\nmod sql;\n",
        );
        assert_eq!(
            check(&fixture),
            [
                "orphan generated file gone.generated.rs has no matching .sql file; delete it and remove its declaration in gone.rs"
            ]
        );
    }

    #[test]
    fn a_stale_file_is_reported_and_conditional_declarations_do_not_count() {
        let fixture = fixture();
        declare_everything(&fixture);
        write(&fixture, "users.generated.rs", "// edited");
        write(
            &fixture,
            "events/more.rs",
            "#[cfg(test)]\n#[path = \"more.generated.rs\"]\nmod sql;\n",
        );
        assert_eq!(
            check(&fixture),
            [
                "events/more.sql has no owner declaration for events/more.generated.rs; add `#[path = \"more.generated.rs\"] mod sql;` to events/more.rs (a declaration under #[cfg] in events/more.rs does not count)",
                "generated file is stale: users.generated.rs; run generate",
            ]
        );
    }

    #[test]
    fn a_declaration_inside_a_cfg_gated_module_does_not_count() {
        let fixture = fixture();
        declare_everything(&fixture);
        write(
            &fixture,
            "events/more.rs",
            "#[cfg(test)]\nmod tests {\n    #[path = \"../../more.generated.rs\"]\n    mod sql;\n}\n",
        );
        assert_eq!(
            check(&fixture),
            [
                "events/more.sql has no owner declaration for events/more.generated.rs; add `#[path = \"more.generated.rs\"] mod sql;` to events/more.rs (a declaration under #[cfg] in events/more.rs does not count)"
            ]
        );
    }

    #[test]
    fn checking_writes_nothing() {
        let fixture = fixture();
        write(&fixture, "gone.generated.rs", "// orphan");
        write(&fixture, "users.generated.rs", "// edited");
        let before = tree(&fixture);
        assert!(!check(&fixture).is_empty());
        assert_eq!(tree(&fixture), before);
    }
}
