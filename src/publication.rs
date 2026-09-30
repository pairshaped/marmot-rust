use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

type Failure = Box<dyn std::error::Error>;

pub(crate) fn publish(
    output: &Path,
    generate: impl FnOnce(&Path) -> Result<(), Failure>,
) -> Result<(), Failure> {
    publish_with_rename(output, generate, |from, to| fs::rename(from, to))
}

fn publish_with_rename(
    output: &Path,
    generate: impl FnOnce(&Path) -> Result<(), Failure>,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), Failure> {
    let output = std::path::absolute(output)?;
    let parent = output
        .parent()
        .ok_or_else(|| std::io::Error::other("output has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".marmot-publish-")
        .tempdir_in(parent)?;
    let staged = temporary.path().join("next");
    fs::create_dir(&staged)?;
    let previous = snapshot(&output)?;
    // Copy, rather than hard-link, because emitters mutate their staged files.
    for (relative, contents) in &previous {
        let source = output.join(relative);
        let destination = staged.join(relative);
        fs::create_dir_all(destination.parent().expect("file has parent"))?;
        let Some(contents) = contents else {
            fs::create_dir_all(&destination)?;
            continue;
        };
        fs::write(&destination, contents)?;
        let metadata = fs::metadata(&source)?;
        fs::set_permissions(&destination, metadata.permissions())?;
        fs::File::open(&destination)?
            .set_times(fs::FileTimes::new().set_modified(metadata.modified()?))?;
    }
    generate(&staged)?;
    if snapshot(&staged)? == previous && output.is_dir() {
        return Ok(());
    }
    let backup = temporary.path().join("previous");
    let had_output = output.exists();
    if had_output {
        rename(&output, &backup)?;
    }
    if let Err(publication) = rename(&staged, &output) {
        if had_output && let Err(restoration) = rename(&backup, &output) {
            let retained = temporary.keep();
            return Err(std::io::Error::other(format!(
                "could not publish {}: {publication}; could not restore: {restoration}; previous output retained at {}",
                output.display(), retained.join("previous").display(),
            )).into());
        }
        return Err(publication.into());
    }
    Ok(())
}

fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>, Failure> {
    if !root.try_exists()? {
        return Ok(BTreeMap::new());
    }
    let mut files = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.path() == root {
            if !entry.file_type().is_dir() {
                return Err(std::io::Error::other("generated output is not a directory").into());
            }
            continue;
        }
        if !entry.file_type().is_file() && !entry.file_type().is_dir() {
            return Err(std::io::Error::other(format!(
                "generated output contains a non-regular file: {}",
                entry.path().display(),
            ))
            .into());
        }
        files.insert(
            entry.path().strip_prefix(root)?.to_path_buf(),
            if entry.file_type().is_dir() {
                None
            } else {
                Some(fs::read(entry.path())?)
            },
        );
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn staging_failure_preserves_the_previous_owner_and_can_recover() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("sql");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("binding.rs"), "old binding").unwrap();
        fs::write(output.join("views.sql"), "old view").unwrap();
        fs::write(output.join("stale.rs"), "stale binding").unwrap();
        let before = fs::metadata(output.join("binding.rs"))
            .unwrap()
            .modified()
            .unwrap();
        let result = publish(&output, |stage| {
            fs::write(stage.join("binding.rs"), "new binding")?;
            fs::remove_file(stage.join("stale.rs"))?;
            Err(std::io::Error::other("view write failed").into())
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(output.join("binding.rs")).unwrap(),
            "old binding"
        );
        assert_eq!(
            fs::read_to_string(output.join("stale.rs")).unwrap(),
            "stale binding"
        );
        assert_eq!(
            fs::metadata(output.join("binding.rs"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        publish(&output, |stage| {
            fs::write(stage.join("binding.rs"), "new binding")?;
            fs::write(stage.join("views.sql"), "new view")?;
            fs::remove_file(stage.join("stale.rs"))?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(output.join("views.sql")).unwrap(),
            "new view"
        );
        assert!(!output.join("stale.rs").exists());
    }
    #[test]
    fn bootstrap_and_noop_preserve_unrelated_file_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("sql");
        publish(&output, |stage| {
            fs::write(stage.join("mod.rs"), "first binding")?;
            Ok(())
        })
        .unwrap();
        fs::write(output.join("README.txt"), "keep me").unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        for file in ["mod.rs", "README.txt"] {
            fs::File::open(output.join(file))
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        publish(&output, |stage| {
            fs::write(stage.join("mod.rs"), "first binding")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            fs::metadata(output.join("mod.rs"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
        publish(&output, |stage| {
            fs::write(stage.join("mod.rs"), "changed binding")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(output.join("README.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            fs::metadata(output.join("README.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
    }

    #[test]
    fn failed_directory_publication_restores_previous_output() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("sql");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("mod.rs"), "old binding").unwrap();
        let before = fs::metadata(output.join("mod.rs"))
            .unwrap()
            .modified()
            .unwrap();
        let mut calls = 0;
        let result = publish_with_rename(
            &output,
            |stage| {
                fs::write(stage.join("mod.rs"), "new binding")?;
                Ok(())
            },
            |from, to| {
                calls += 1;
                if calls == 2 {
                    Err(std::io::Error::other("publication refused"))
                } else {
                    fs::rename(from, to)
                }
            },
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("publication refused")
        );
        assert_eq!(
            fs::read_to_string(output.join("mod.rs")).unwrap(),
            "old binding"
        );
        assert_eq!(
            fs::metadata(output.join("mod.rs"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_restoration_reports_and_retains_the_previous_output() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("sql");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("mod.rs"), "old binding").unwrap();
        let mut calls = 0;
        let result = publish_with_rename(
            &output,
            |stage| {
                fs::write(stage.join("mod.rs"), "new binding")?;
                Ok(())
            },
            |from, to| {
                calls += 1;
                if calls > 1 {
                    Err(std::io::Error::other("rename refused"))
                } else {
                    fs::rename(from, to)
                }
            },
        );
        let error = result.unwrap_err().to_string();
        let retained = PathBuf::from(error.split("previous output retained at ").nth(1).unwrap());
        assert_eq!(
            fs::read_to_string(retained.join("mod.rs")).unwrap(),
            "old binding"
        );
        assert!(!output.exists());
    }
}
