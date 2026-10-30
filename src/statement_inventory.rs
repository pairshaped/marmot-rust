use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    process::Command,
};

use serde::Serialize;
use sha2::{Digest, Sha256};

type InventoryResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Request {
    pub repository: PathBuf,
    pub platform_root: PathBuf,
    pub base: String,
    pub owner_configs: Vec<PathBuf>,
    pub before_owner_configs: Option<Vec<PathBuf>>,
}

#[derive(Debug, Serialize)]
pub struct Version {
    pub statement: String,
    pub commit: String,
    pub sql_file: String,
    pub source_sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Change {
    pub statement: String,
    pub column: Option<String>,
    pub before: Option<Version>,
    pub after: Option<Version>,
}

#[derive(Debug, Serialize)]
pub struct Inventory {
    pub base_commit: String,
    pub candidate_commit: String,
    pub changes: Vec<Change>,
}

fn git(repository: &Path, args: &[&str]) -> InventoryResult<Vec<u8>> {
    let output = Command::new("git")
        .args(["--no-pager", "--literal-pathspecs"])
        .args(args)
        .current_dir(repository)
        .output()?;
    if !output.status.success() {
        return Err("Could not read committed SQL inventory from Git".into());
    }
    Ok(output.stdout)
}

fn revision(repository: &Path, name: &str) -> InventoryResult<String> {
    let output = git(
        repository,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{name}^{{commit}}"),
        ],
    )?;
    let commit = String::from_utf8(output)?.trim().to_owned();
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Git inventory requires an exact commit identity".into());
    }
    Ok(commit)
}

fn relative(path: &Path) -> InventoryResult<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            _ => {
                return Err(
                    "SQL inventory paths must stay inside the repository and platform".into(),
                );
            }
        }
    }
    Ok(result)
}

fn paths(bytes: Vec<u8>) -> InventoryResult<BTreeSet<PathBuf>> {
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| Ok(PathBuf::from(std::str::from_utf8(part)?)))
        .collect()
}

fn git_pathspec(path: &Path) -> InventoryResult<&str> {
    let value = path.to_str().ok_or("Platform path must be UTF-8")?;
    Ok(if value.is_empty() { "." } else { value })
}

fn tree(request: &Request, commit: &str, platform: &Path) -> InventoryResult<BTreeSet<PathBuf>> {
    paths(git(
        &request.repository,
        &[
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            commit,
            "--",
            git_pathspec(platform)?,
        ],
    )?)
}

fn blob(request: &Request, commit: &str, path: &Path) -> InventoryResult<String> {
    let spec = format!(
        "{commit}:{}",
        path.to_str().ok_or("SQL source path must be UTF-8")?
    );
    Ok(String::from_utf8(git(
        &request.repository,
        &["show", "--no-textconv", &spec],
    )?)?)
}

fn renames(
    request: &Request,
    before: &str,
    after: &str,
    platform: &Path,
) -> InventoryResult<Vec<(PathBuf, PathBuf)>> {
    let output = git(
        &request.repository,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--name-status",
            "-z",
            "--find-renames",
            before,
            after,
            "--",
            git_pathspec(platform)?,
        ],
    )?;
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()?;
    let mut index = 0;
    let mut result = Vec::new();
    while let Some(status) = fields.get(index) {
        let old = fields
            .get(index + 1)
            .ok_or("Incomplete Git change metadata")?;
        index += 2;
        if status.starts_with(['R', 'C']) {
            let new = fields.get(index).ok_or("Incomplete Git rename metadata")?;
            index += 1;
            if status.starts_with('R') {
                result.push((PathBuf::from(old), PathBuf::from(new)));
            }
        }
    }
    Ok(result)
}

fn roots(
    request: &Request,
    commit: &str,
    platform: &Path,
    files: &BTreeSet<PathBuf>,
    owners: &[PathBuf],
) -> InventoryResult<BTreeSet<PathBuf>> {
    let mut roots = BTreeSet::new();
    for owner in owners {
        let path = platform.join(relative(owner)?);
        if !files.contains(&path) {
            continue;
        }
        let config = crate::FileConfig::from_toml_str(&blob(request, commit, &path)?)
            .map_err(|_| "Invalid declared SQL owner configuration")?;
        roots.insert(platform.join(relative(
            config.source_root.as_deref().unwrap_or(Path::new("src")),
        )?));
        for database in config.databases.values() {
            if let Some(source) = &database.source_root {
                roots.insert(platform.join(relative(source)?));
            }
        }
    }
    Ok(roots)
}

fn owned(path: &Path, roots: &BTreeSet<PathBuf>, files: &BTreeSet<PathBuf>) -> bool {
    path.extension().is_some_and(|ext| ext == "sql")
        && !path
            .components()
            .any(|part| part.as_os_str() == "generated")
        && files.contains(&path.with_extension("rs"))
        && roots.iter().any(|root| {
            path.starts_with(root)
                && !path
                    .parent()
                    .into_iter()
                    .flat_map(Path::ancestors)
                    .take_while(|ancestor| *ancestor != root.as_path())
                    .any(|ancestor| files.contains(&ancestor.join("marmot.toml")))
        })
}

fn statements(
    request: &Request,
    commit: &str,
    platform: &Path,
    file: &Path,
) -> InventoryResult<BTreeMap<(String, Option<String>), Version>> {
    let source = blob(request, commit, file)?;
    let blocks = crate::sqlite::blocks::parse_sql_blocks(&source)
        .map_err(|_| "Invalid raw companion SQL declarations")?;
    let label_file = file
        .strip_prefix(platform)?
        .to_str()
        .ok_or("SQL label must be UTF-8")?;
    let mut statements = BTreeMap::new();
    for block in blocks {
        let columns = block
            .column_substitution
            .as_ref()
            .map(|columns| {
                columns
                    .choices
                    .iter()
                    .map(|column| Some(column.as_str()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![None]);
        for column in columns {
            let sql = crate::sql_text::prepare_companion_sql(&block, column)?;
            statements.insert(
                (block.function_name.clone(), column.map(str::to_owned)),
                Version {
                    statement: format!("{label_file}#{}", block.function_name),
                    commit: commit.to_owned(),
                    sql_file: file.to_str().ok_or("SQL path must be UTF-8")?.to_owned(),
                    source_sha256: format!("{:x}", Sha256::digest(sql.as_bytes())),
                },
            );
        }
    }
    Ok(statements)
}

pub fn run(request: &Request) -> InventoryResult<Inventory> {
    if request.owner_configs.is_empty() {
        return Err("Declare at least one SQL owner for inventory".into());
    }
    let platform = relative(&request.platform_root)?;
    let base_commit = revision(&request.repository, &request.base)?;
    let candidate_commit = revision(&request.repository, "HEAD")?;
    let before_files = tree(request, &base_commit, &platform)?;
    let after_files = tree(request, &candidate_commit, &platform)?;
    let before_owners = request
        .before_owner_configs
        .as_deref()
        .unwrap_or(&request.owner_configs);
    for owner in request.owner_configs.iter().chain(before_owners) {
        let path = platform.join(relative(owner)?);
        if !before_files.contains(&path) && !after_files.contains(&path) {
            return Err("A declared SQL owner is absent from both revisions".into());
        }
    }
    let before_roots = roots(
        request,
        &base_commit,
        &platform,
        &before_files,
        before_owners,
    )?;
    let after_roots = roots(
        request,
        &candidate_commit,
        &platform,
        &after_files,
        &request.owner_configs,
    )?;
    if after_roots.is_empty() {
        return Err("Declared SQL owners are absent from the candidate revision".into());
    }
    let changed = paths(git(
        &request.repository,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            &base_commit,
            &candidate_commit,
            "--",
            git_pathspec(&platform)?,
        ],
    )?)?;
    let mut changes = Vec::new();
    let mut pairs = Vec::new();
    let mut paired = BTreeSet::new();
    for (old, new) in renames(request, &base_commit, &candidate_commit, &platform)? {
        if owned(&old, &before_roots, &before_files) && owned(&new, &after_roots, &after_files) {
            paired.insert(old.clone());
            paired.insert(new.clone());
            pairs.push((Some(old), Some(new)));
        }
    }
    for file in before_files.union(&after_files) {
        if paired.contains(file) {
            continue;
        }
        let before_owned = owned(file, &before_roots, &before_files);
        let after_owned = owned(file, &after_roots, &after_files);
        if !(before_owned || after_owned)
            || (!changed.contains(file) && before_owned == after_owned)
        {
            continue;
        }
        pairs.push((
            before_owned.then(|| file.clone()),
            after_owned.then(|| file.clone()),
        ));
    }
    for (old_file, new_file) in pairs {
        let mut before = if let Some(file) = old_file {
            statements(request, &base_commit, &platform, &file)?
        } else {
            BTreeMap::new()
        };
        let mut after = if let Some(file) = new_file {
            statements(request, &candidate_commit, &platform, &file)?
        } else {
            BTreeMap::new()
        };
        let labels = before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for key in labels {
            let old = before.remove(&key);
            let new = after.remove(&key);
            if old.as_ref().zip(new.as_ref()).is_some_and(|(old, new)| {
                old.source_sha256 == new.source_sha256 && old.statement == new.statement
            }) {
                continue;
            }
            changes.push(Change {
                statement: new
                    .as_ref()
                    .or(old.as_ref())
                    .ok_or("Empty statement change")?
                    .statement
                    .clone(),
                column: key.1,
                before: old,
                after: new,
            });
        }
    }
    Ok(Inventory {
        base_commit,
        candidate_commit,
        changes,
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use super::*;

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repository)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn inventory_finds_only_changed_owned_raw_statements_without_a_database() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::create_dir_all(root.join("app/db/migrations")).unwrap();
        fs::write(
            root.join("app/marmot.toml"),
            "[tools.marmot]\nsource_root='src'\ndatabase='db/must-not-open.db'\n",
        )
        .unwrap();
        fs::write(root.join("app/src/people.rs"), "").unwrap();
        fs::write(
            root.join("app/src/people.generated.rs"),
            "// never read compiled bindings",
        )
        .unwrap();
        fs::write(
            root.join("app/src/people.sql"),
            "-- func: unchanged\nselect 1\n\n-- func: selected\nselect id from people\n\n-- func: retired\nselect name from people\n",
        )
        .unwrap();
        git(root, &["init", "-b", "master"]);
        git(root, &["config", "user.name", "Dave Rapin"]);
        git(root, &["config", "user.email", "dave@rapin.com"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "Before fixture"]);
        let base = git(root, &["rev-parse", "HEAD"]);
        fs::write(root.join("app/src/people.sql"), "-- func: unchanged\nselect 1\n\n-- func: selected\nselect id from people where id=@id\n\n-- func: chosen\n-- columns: id, name\nselect {{column}} from people\n").unwrap();
        fs::write(
            root.join("app/src/people.generated.rs"),
            "// unrelated generated binding edits",
        )
        .unwrap();
        fs::write(
            root.join("app/db/migrations/001_example.sql"),
            "create table example(id integer);",
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "After fixture"]);
        let candidate = git(root, &["rev-parse", "HEAD"]);
        let inventory = run(&Request {
            repository: root.into(),
            platform_root: "app".into(),
            base: base.clone(),
            owner_configs: vec!["marmot.toml".into()],
            before_owner_configs: None,
        })
        .unwrap();
        assert_eq!(inventory.base_commit, base);
        assert_eq!(inventory.candidate_commit, candidate);
        assert_eq!(inventory.changes.len(), 4);
        let change = inventory
            .changes
            .iter()
            .find(|change| change.statement == "src/people.sql#selected")
            .unwrap();
        assert_eq!(change.statement, "src/people.sql#selected");
        assert_ne!(
            change.before.as_ref().unwrap().source_sha256,
            change.after.as_ref().unwrap().source_sha256
        );
        let retired = inventory
            .changes
            .iter()
            .find(|change| change.statement == "src/people.sql#retired")
            .unwrap();
        assert!(retired.before.is_some() && retired.after.is_none());
        let added = inventory
            .changes
            .iter()
            .filter(|change| change.statement == "src/people.sql#chosen")
            .collect::<Vec<_>>();
        assert_eq!(added.len(), 2);
        assert!(
            added
                .iter()
                .all(|change| change.before.is_none() && change.after.is_some())
        );
        assert_eq!(
            added
                .iter()
                .filter_map(|change| change.column.as_deref())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["id", "name"])
        );
        assert!(!root.join("app/db/must-not-open.db").exists());
        assert!(!root.join("target").exists());
        for invalid in ["missing.toml", "../outside.toml"] {
            assert!(
                run(&Request {
                    repository: root.into(),
                    platform_root: "app".into(),
                    base: base.clone(),
                    owner_configs: vec!["marmot.toml".into(), invalid.into()],
                    before_owner_configs: None,
                })
                .is_err(),
                "unknown or escaping owner declarations must fail closed"
            );
        }
        fs::rename(
            root.join("app/src/people.sql"),
            root.join("app/src/renamed.sql"),
        )
        .unwrap();
        fs::rename(
            root.join("app/src/people.rs"),
            root.join("app/src/renamed.rs"),
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "Rename fixture"]);
        let renamed = run(&Request {
            repository: root.into(),
            platform_root: "app".into(),
            base: candidate,
            owner_configs: vec!["marmot.toml".into()],
            before_owner_configs: None,
        })
        .unwrap();
        assert_eq!(
            renamed.changes.len(),
            4,
            "a file move retains before/after statement pairs"
        );
        assert!(renamed.changes.iter().all(|change| {
            change.statement.starts_with("src/renamed.sql#")
                && change
                    .before
                    .as_ref()
                    .zip(change.after.as_ref())
                    .is_some_and(|(before, after)| {
                        before.sql_file == "app/src/people.sql"
                            && after.sql_file == "app/src/renamed.sql"
                            && before.source_sha256 == after.source_sha256
                    })
        }));
        let before_choices = git(root, &["rev-parse", "HEAD"]);
        let renamed_sql = root.join("app/src/renamed.sql");
        let source = fs::read_to_string(&renamed_sql).unwrap();
        fs::write(
            &renamed_sql,
            source.replace(
                "-- columns: id, name\nselect {{column}} from people",
                "-- columns: id, email\nselect {{column}} from people where active=1",
            ),
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "Change column choices fixture"]);
        let choices = run(&Request {
            repository: root.into(),
            platform_root: "app".into(),
            base: before_choices,
            owner_configs: vec!["marmot.toml".into()],
            before_owner_configs: None,
        })
        .unwrap();
        assert_eq!(choices.changes.len(), 3);
        for change in &choices.changes {
            assert_eq!(change.statement, "src/renamed.sql#chosen");
            match change.column.as_deref() {
                Some("id") => assert_ne!(
                    change.before.as_ref().unwrap().source_sha256,
                    change.after.as_ref().unwrap().source_sha256
                ),
                Some("name") => assert!(change.before.is_some() && change.after.is_none()),
                Some("email") => assert!(change.before.is_none() && change.after.is_some()),
                other => panic!("Unexpected changed column: {other:?}"),
            }
        }
        fs::create_dir_all(root.join("app/src/nested")).unwrap();
        fs::write(
            root.join("app/src/nested/marmot.toml"),
            "[tools.marmot]\nsource_root='src/nested'\n",
        )
        .unwrap();
        fs::write(root.join("app/src/nested/inner.rs"), "").unwrap();
        fs::write(
            root.join("app/src/nested/inner.sql"),
            "-- func: inner\nselect 42\n",
        )
        .unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "Unregistered nested owner fixture"]);
        let before_registration = git(root, &["rev-parse", "HEAD"]);
        fs::write(root.join("app/registry.txt"), "register nested owner\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "Register nested owner fixture"]);
        let registered = run(&Request {
            repository: root.into(),
            platform_root: "app".into(),
            base: before_registration,
            owner_configs: vec!["marmot.toml".into(), "src/nested/marmot.toml".into()],
            before_owner_configs: Some(vec!["marmot.toml".into()]),
        })
        .unwrap();
        assert_eq!(
            registered.changes.len(),
            1,
            "newly registered raw SQL must be measured even when its text did not change"
        );
        assert_eq!(
            registered.changes[0].statement,
            "src/nested/inner.sql#inner"
        );
        assert!(registered.changes[0].before.is_none() && registered.changes[0].after.is_some());
    }
}
