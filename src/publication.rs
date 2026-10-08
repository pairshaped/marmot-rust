use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) type Failure = Box<dyn std::error::Error>;

/// The complete set of files one generation owns.
///
/// `desired` holds every file with its final contents. `managed` holds the
/// existing files Marmot is allowed to delete; any managed file missing from
/// `desired` is stale.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub(crate) desired: BTreeMap<PathBuf, String>,
    pub(crate) managed: BTreeSet<PathBuf>,
    /// Directories emptied by removals are pruned up to, but not including, this one.
    pub(crate) prune_root: Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Changes {
    pub(crate) writes: BTreeMap<PathBuf, String>,
    pub(crate) removals: BTreeSet<PathBuf>,
}

impl Changes {
    pub(crate) fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.removals.is_empty()
    }

    /// Every stale path, in path order.
    pub(crate) fn paths(&self) -> BTreeSet<&Path> {
        self.writes
            .keys()
            .chain(&self.removals)
            .map(PathBuf::as_path)
            .collect()
    }
}

impl Plan {
    /// Compare the plan with the disk. Reads only; never touches a path.
    pub(crate) fn changes(&self) -> io::Result<Changes> {
        let mut changes = Changes::default();
        for (path, content) in &self.desired {
            match fs::read(path) {
                Ok(current) if current == content.as_bytes() => {}
                Ok(_) => {
                    changes.writes.insert(path.clone(), content.clone());
                }
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    changes.writes.insert(path.clone(), content.clone());
                }
                Err(source) => return Err(source),
            }
        }
        for path in &self.managed {
            if !self.desired.contains_key(path) && path.try_exists()? {
                changes.removals.insert(path.clone());
            }
        }
        Ok(changes)
    }
}

/// Publish a plan. Every changed file is staged first. Directories are then
/// published one at a time; if any step fails, everything already published is
/// restored so the prior complete output survives. Publication is not atomic
/// across directories: a process killed mid-publish (SIGKILL, power loss) can
/// leave a partly published tree, with the replaced files in the
/// `.marmot-publish-*` folder under `stage_parent`. Rerun generation to repair.
pub(crate) fn publish(plan: &Plan, stage_parent: &Path) -> Result<(), Failure> {
    publish_with_rename(plan, stage_parent, |from, to| fs::rename(from, to))
}

enum Action {
    Write { target: PathBuf, staged: PathBuf },
    Remove { target: PathBuf },
}

impl Action {
    fn target(&self) -> &Path {
        match self {
            Self::Write { target, .. } | Self::Remove { target } => target,
        }
    }
}

#[derive(Default)]
struct Journal {
    entries: Vec<Entry>,
    created_dirs: Vec<PathBuf>,
}

struct Entry {
    target: PathBuf,
    backup: Option<PathBuf>,
    wrote: bool,
}

fn publish_with_rename(
    plan: &Plan,
    stage_parent: &Path,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<(), Failure> {
    let changes = plan.changes()?;
    if changes.is_empty() {
        return Ok(());
    }
    let stage_parent = std::path::absolute(stage_parent)?;
    fs::create_dir_all(&stage_parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".marmot-publish-")
        .tempdir_in(&stage_parent)?;
    let next = temporary.path().join("next");
    let previous = temporary.path().join("previous");
    fs::create_dir(&next)?;
    fs::create_dir(&previous)?;

    let mut by_directory: BTreeMap<PathBuf, Vec<Action>> = BTreeMap::new();
    for (index, (target, content)) in changes.writes.iter().enumerate() {
        let staged = next.join(index.to_string());
        fs::write(&staged, content)?;
        if let Ok(metadata) = fs::metadata(target) {
            fs::set_permissions(&staged, metadata.permissions())?;
        }
        by_directory
            .entry(parent_of(target))
            .or_default()
            .push(Action::Write {
                target: target.clone(),
                staged,
            });
    }
    for target in &changes.removals {
        by_directory
            .entry(parent_of(target))
            .or_default()
            .push(Action::Remove {
                target: target.clone(),
            });
    }

    let mut journal = Journal::default();
    let mut backups = 0usize;
    let mut apply = || -> io::Result<()> {
        for actions in by_directory.values() {
            for action in actions {
                let target = action.target();
                if let Some(directory) = target.parent() {
                    create_missing_directories(directory, &mut journal)?;
                }
                let backup = if target.try_exists()? {
                    backups += 1;
                    let backup = previous.join(backups.to_string());
                    rename(target, &backup)?;
                    Some(backup)
                } else {
                    None
                };
                journal.entries.push(Entry {
                    target: target.to_path_buf(),
                    backup,
                    wrote: false,
                });
                if let Action::Write { staged, target } = action {
                    rename(staged, target)?;
                    journal.entries.last_mut().expect("entry pushed").wrote = true;
                }
            }
        }
        Ok(())
    };
    if let Err(publication) = apply() {
        let mut failures = Vec::new();
        for entry in journal.entries.iter().rev() {
            if entry.wrote
                && let Err(error) = fs::remove_file(&entry.target)
            {
                failures.push(format!("{}: {error}", entry.target.display()));
                continue;
            }
            if let Some(backup) = &entry.backup
                && let Err(error) = rename(backup, &entry.target)
            {
                failures.push(format!("{}: {error}", entry.target.display()));
            }
        }
        for directory in journal.created_dirs.iter().rev() {
            let _ = fs::remove_dir(directory);
        }
        if failures.is_empty() {
            return Err(publication.into());
        }
        let retained = temporary.keep();
        return Err(io::Error::other(format!(
            "could not publish generated files: {publication}; could not restore {}; previous files retained at {}",
            failures.join(", "),
            retained.join("previous").display(),
        ))
        .into());
    }

    if let Some(root) = &plan.prune_root {
        for removed in &changes.removals {
            for directory in removed.ancestors().skip(1) {
                if directory == root || !directory.starts_with(root) {
                    break;
                }
                if fs::remove_dir(directory).is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

fn create_missing_directories(directory: &Path, journal: &mut Journal) -> io::Result<()> {
    let mut missing = Vec::new();
    for ancestor in directory.ancestors() {
        if ancestor.as_os_str().is_empty() || ancestor.try_exists()? {
            break;
        }
        missing.push(ancestor.to_path_buf());
    }
    for path in missing.into_iter().rev() {
        fs::create_dir(&path)?;
        journal.created_dirs.push(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn old_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_600_000_000)
    }

    fn set_old(path: &Path) {
        fs::File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old_time()))
            .unwrap();
    }

    fn modified(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn plan(files: &[(&Path, &str)]) -> Plan {
        Plan {
            desired: files
                .iter()
                .map(|(path, content)| (path.to_path_buf(), content.to_string()))
                .collect(),
            ..Plan::default()
        }
    }

    #[test]
    fn unchanged_plan_touches_nothing_and_changed_plan_updates_only_affected_files() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (temp.path().join("a/one.rs"), temp.path().join("b/two.rs"));
        publish(&plan(&[(&a, "one"), (&b, "two")]), temp.path()).unwrap();
        set_old(&a);
        set_old(&b);
        publish(&plan(&[(&a, "one"), (&b, "two")]), temp.path()).unwrap();
        assert_eq!((modified(&a), modified(&b)), (old_time(), old_time()));
        publish(&plan(&[(&a, "one"), (&b, "changed")]), temp.path()).unwrap();
        assert_eq!(modified(&a), old_time());
        assert_ne!(modified(&b), old_time());
        assert_eq!(fs::read_to_string(&b).unwrap(), "changed");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[test]
    fn failure_at_the_second_directory_restores_the_first() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b, stale) = (
            temp.path().join("a/one.rs"),
            temp.path().join("b/two.rs"),
            temp.path().join("c/stale.rs"),
        );
        publish(
            &plan(&[(&a, "old one"), (&b, "old two"), (&stale, "stale")]),
            temp.path(),
        )
        .unwrap();
        for path in [&a, &b, &stale] {
            set_old(path);
        }
        let fresh = temp.path().join("a/fresh.rs");
        let mut next = plan(&[(&a, "new one"), (&b, "new two"), (&fresh, "fresh")]);
        next.managed.insert(stale.clone());
        let mut failed = false;
        let result = publish_with_rename(&next, temp.path(), |from, to| {
            if !failed && to == b {
                failed = true;
                return Err(io::Error::other("second directory refused"));
            }
            fs::rename(from, to)
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("second directory refused")
        );
        assert_eq!(fs::read_to_string(&a).unwrap(), "old one");
        assert_eq!(fs::read_to_string(&b).unwrap(), "old two");
        assert_eq!(fs::read_to_string(&stale).unwrap(), "stale");
        assert!(!fresh.exists());
        for path in [&a, &b, &stale] {
            assert_eq!(modified(path), old_time());
        }
        let leftovers = fs::read_dir(temp.path()).unwrap().count();
        assert_eq!(leftovers, 3, "only a, b and c remain");
    }

    #[test]
    fn removals_prune_emptied_directories_below_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("shared");
        let nested = root.join("pages/users.rs");
        publish(&plan(&[(&nested, "old")]), temp.path()).unwrap();
        let mut next = plan(&[]);
        next.managed.insert(nested.clone());
        next.prune_root = Some(root.clone());
        publish(&next, temp.path()).unwrap();
        assert!(root.is_dir());
        assert!(!root.join("pages").exists());
    }

    #[test]
    fn failed_restoration_reports_and_retains_the_previous_files() {
        let temp = tempfile::tempdir().unwrap();
        let (a, b) = (temp.path().join("a/one.rs"), temp.path().join("b/two.rs"));
        publish(&plan(&[(&a, "old one"), (&b, "old two")]), temp.path()).unwrap();
        let next = plan(&[(&a, "new one"), (&b, "new two")]);
        let mut calls = 0;
        let result = publish_with_rename(&next, temp.path(), |from, to| {
            calls += 1;
            if calls > 3 {
                Err(io::Error::other("rename refused"))
            } else {
                fs::rename(from, to)
            }
        });
        let error = result.unwrap_err().to_string();
        let retained = PathBuf::from(error.split("previous files retained at ").nth(1).unwrap());
        assert_eq!(fs::read_to_string(retained.join("1")).unwrap(), "old one");
    }

    #[test]
    fn changes_reads_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let (kept, stale) = (temp.path().join("kept.rs"), temp.path().join("stale.rs"));
        fs::write(&kept, "kept").unwrap();
        fs::write(&stale, "stale").unwrap();
        let mut next = plan(&[(&kept, "kept"), (&temp.path().join("new.rs"), "new")]);
        next.managed.insert(stale.clone());
        let changes = next.changes().unwrap();
        assert_eq!(changes.writes.len(), 1);
        assert_eq!(changes.removals, BTreeSet::from([stale]));
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }
}
