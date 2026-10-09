//! CLI generation receipts. A cache is disposable and never authorizes check mode.
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::AnalysisTarget;
use marmot::{FileConfig, discovery, emit, views};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct State {
    version: u32,
    fingerprint: String,
    pub inputs: String,
    pub outputs: BTreeMap<PathBuf, String>,
}

pub(super) struct Cache {
    path: PathBuf,
}

impl Cache {
    pub fn new(target: &AnalysisTarget) -> Self {
        let source = &target.config.source_root;
        let key = format!(
            "{:?}",
            (source, &target.config.database, &target.config.output)
        );
        Self {
            path: source
                .parent()
                .unwrap_or(source)
                .join(".local/marmot")
                .join(format!("{}.json", digest(key.as_bytes()))),
        }
    }

    pub fn current(
        &self,
        target: &AnalysisTarget,
        config: &FileConfig,
        config_path: &Path,
        identity: &str,
    ) -> Result<State> {
        let mut input = Fingerprint::default();
        input.add(b"marmot-generation-v1");
        input.add(identity.as_bytes());
        input.add(format!("{target:?}{config:?}").as_bytes());
        input.optional_file(config_path)?;
        for file in discovery::discover_sql_files(&target.config.source_root)? {
            input.file(&file.path)?;
            input.add(
                file.path
                    .with_extension("rs")
                    .as_os_str()
                    .as_encoded_bytes(),
            );
        }
        for root in [&target.view_source_root, &target.config.source_root] {
            input.sql_tree(&root.join("db_views"))?;
        }
        for root in &target.auxiliary_inputs {
            input.sql_tree(root)?;
        }
        if let Some(path) = &target.init_sql {
            input.file(path)?;
        }

        let database = &target.config.database;
        let metadata = fs::metadata(database)?;
        // Birth time distinguishes a recreated database with identical schema.
        // Filesystems without it fall back to full generation.
        input.add(format!("{:?}", metadata.created()?).as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            input.add(&metadata.dev().to_le_bytes());
            input.add(&metadata.ino().to_le_bytes());
        }
        let inputs = format!("{:x}", input.0.clone().finalize());
        let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut statement = connection
            .prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name")?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })? {
            input.add(serde_json::to_string(&row?)?.as_bytes());
        }
        let mut outputs = emit::managed_files(&target.config)?;
        let views = target.config.output.join(views::GENERATED_FILE);
        if views.try_exists()? {
            outputs.insert(views);
        }
        let outputs = outputs
            .into_iter()
            .map(|path| {
                let hash = digest(&fs::read(&path)?);
                Ok((path, hash))
            })
            .collect::<io::Result<_>>()?;
        Ok(State {
            version: 1,
            fingerprint: input.finish(),
            inputs,
            outputs,
        })
    }

    pub fn matches(&self, current: &State) -> bool {
        fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok())
            .as_ref()
            == Some(current)
    }

    pub fn save(&self, state: &State) -> Result<()> {
        let parent = self.path.parent().expect("cache parent");
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(temporary.as_file_mut(), state)?;
        temporary.flush()?;
        temporary.persist(&self.path)?;
        Ok(())
    }
}

pub(super) fn executable_identity() -> Result<String> {
    Ok(digest(&fs::read(std::env::current_exe()?)?))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Default)]
struct Fingerprint(Sha256);

impl Fingerprint {
    fn add(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    fn file(&mut self, path: &Path) -> io::Result<()> {
        self.add(path.as_os_str().as_encoded_bytes());
        self.add(&fs::read(path)?);
        Ok(())
    }

    fn optional_file(&mut self, path: &Path) -> io::Result<()> {
        self.add(path.as_os_str().as_encoded_bytes());
        if path.try_exists()? {
            self.file(path)?;
        }
        Ok(())
    }

    fn sql_tree(&mut self, root: &Path) -> Result<()> {
        self.add(root.as_os_str().as_encoded_bytes());
        if !root.try_exists()? {
            return Ok(());
        }
        for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
            let entry = entry?;
            if entry.file_type().is_file()
                && entry.path().extension().is_some_and(|ext| ext == "sql")
            {
                self.file(entry.path())?;
            }
        }
        Ok(())
    }

    fn finish(self) -> String {
        format!("{:x}", self.0.finalize())
    }
}
