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
    schema: String,
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
        input.add(b"marmot-generation-v2");
        input.add(identity.as_bytes());
        input.add(format!("{target:?}{config:?}").as_bytes());
        input.optional_file(config_path)?;
        for path in marmot::value_sets::input_paths(&target.config.source_root)? {
            input.optional_file(&path)?;
        }
        for path in marmot::scoped_ids::input_paths(&target.config.source_root)? {
            input.optional_file(&path)?;
        }
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
        let schema = marmot::schema::fingerprint(&connection)?;
        input.add(schema.as_bytes());
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
            version: 2,
            fingerprint: input.finish(),
            inputs,
            schema,
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

    pub fn publish_analyzed(
        &self,
        target: &AnalysisTarget,
        config: &FileConfig,
        config_path: &Path,
        identity: &str,
        initial_inputs: Option<&str>,
        analyzed_schema: &str,
    ) -> Result<()> {
        let state = self.current(target, config, config_path, identity)?;
        if initial_inputs == Some(state.inputs.as_str()) && state.schema == analyzed_schema {
            self.save(&state)?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use marmot::{Config, Target};

    #[test]
    fn toggling_statement_labels_changes_the_generation_inputs() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("items.rs"), "").unwrap();
        fs::write(
            source.join("items.sql"),
            "-- func: list_items\nselect * from items",
        )
        .unwrap();
        let database = root.path().join("app.sqlite");
        Connection::open(&database)
            .unwrap()
            .execute_batch("create table items (id integer primary key) strict;")
            .unwrap();
        let target = |statement_labels| AnalysisTarget {
            config: Config {
                database: database.clone(),
                source_root: source.clone(),
                output: source.join("generated/sql"),
                target: Target::Rust,
                check: false,
                temporal: Default::default(),
            },
            init_sql: None,
            view_source_root: source.clone(),
            serialize_modules: Default::default(),
            public_bindings: None,
            auxiliary_inputs: vec![],
            statement_labels,
        };
        let config = FileConfig::default();
        let config_path = root.path().join("marmot.toml");
        let inputs = |statement_labels| {
            let target = target(statement_labels);
            Cache::new(&target)
                .current(&target, &config, &config_path, "executable")
                .unwrap()
                .inputs
        };
        assert_eq!(inputs(false), inputs(false));
        assert_ne!(
            inputs(false),
            inputs(true),
            "a labelled run must never skip regeneration on unlabelled bindings"
        );
    }

    #[test]
    fn schema_change_after_analysis_cannot_authorize_old_bindings() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("src");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("items.rs"),
            "#[path = \"items.generated.rs\"] mod sql;\n",
        )
        .unwrap();
        fs::write(
            source.join("items.sql"),
            "-- func: list_items\nselect * from items",
        )
        .unwrap();
        let database = root.path().join("app.sqlite");
        let template = Connection::open_in_memory().unwrap();
        template
            .execute_batch(
                "create table items (id integer primary key, name text not null) strict;",
            )
            .unwrap();
        template
            .execute("vacuum into ?1", [database.to_str().unwrap()])
            .unwrap();
        let target = AnalysisTarget {
            config: Config {
                database: database.clone(),
                source_root: source.clone(),
                output: source.join("generated/sql"),
                target: Target::Rust,
                check: false,
                temporal: Default::default(),
            },
            init_sql: None,
            view_source_root: source.clone(),
            serialize_modules: Default::default(),
            public_bindings: None,
            auxiliary_inputs: vec![],
            statement_labels: false,
        };
        let config = FileConfig::default();
        let config_path = root.path().join("marmot.toml");
        let cache = Cache::new(&target);
        let initial = cache
            .current(&target, &config, &config_path, "executable")
            .unwrap();
        let analyzed =
            marmot::analyzer::analyze_project_snapshot(&target.config, None, &source).unwrap();
        assert_eq!(analyzed.project.queries[0].columns.len(), 2);
        Connection::open(&database)
            .unwrap()
            .execute_batch("alter table items add column extra text;")
            .unwrap();
        marmot::emit_project(&target.config, &analyzed.project).unwrap();
        cache
            .publish_analyzed(
                &target,
                &config,
                &config_path,
                "executable",
                Some(&initial.inputs),
                &analyzed.schema_fingerprint,
            )
            .unwrap();
        assert!(
            !cache.path.exists(),
            "a receipt must not pair the new schema with old bindings"
        );
        assert!(
            !cache.matches(
                &cache
                    .current(&target, &config, &config_path, "executable")
                    .unwrap()
            )
        );

        let fresh =
            marmot::analyzer::analyze_project_snapshot(&target.config, None, &source).unwrap();
        assert_eq!(fresh.project.queries[0].columns.len(), 3);
        marmot::emit_project(&target.config, &fresh.project).unwrap();
        cache
            .publish_analyzed(
                &target,
                &config,
                &config_path,
                "executable",
                Some(&initial.inputs),
                &fresh.schema_fingerprint,
            )
            .unwrap();
        assert!(
            cache.matches(
                &cache
                    .current(&target, &config, &config_path, "executable")
                    .unwrap()
            )
        );
    }
}
