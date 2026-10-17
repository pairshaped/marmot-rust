mod rust;
mod visibility;

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::config::{Config, Target};
use crate::error::{Error, Result};
use crate::model::Project;
use crate::publication::{self, Plan};
use crate::views::{self, ViewDefinition};

pub use rust::output_paths;

/// Every Rust file Marmot may delete: any adjacent binding under the source
/// root and any Rust file in the shared output folder, such as a retired
/// central tree.
pub fn managed_files(config: &Config) -> Result<BTreeSet<PathBuf>> {
    let mut managed = BTreeSet::new();
    for (root, adjacent_only) in [(&config.source_root, true), (&config.output, false)] {
        if !root.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !entry.file_type().is_dir()
                    || !entry.path().join("marmot.toml").is_file()
            })
        {
            let entry = entry.map_err(|source| Error::WalkDir {
                path: root.to_path_buf(),
                source,
            })?;
            let name = entry.file_name().to_string_lossy();
            let owned = if adjacent_only {
                name.ends_with(".generated.rs")
            } else {
                name.ends_with(".rs") || name.ends_with(".generated.rs")
            };
            if entry.file_type().is_file() && owned {
                managed.insert(entry.path().to_path_buf());
            }
        }
    }
    Ok(managed)
}

pub fn emit_project(config: &Config, project: &Project) -> Result<()> {
    emit_project_with_serialize_modules(config, project, &BTreeSet::new())
}

pub fn emit_project_with_serialize_modules(
    config: &Config,
    project: &Project,
    serialize_modules: &BTreeSet<String>,
) -> Result<()> {
    emit(config, project, serialize_modules, None, None, false)
}

/// Publish one owner's bindings and view installation SQL as one complete
/// generation. Nothing is published until both have rendered successfully.
pub fn emit_project_with_views(
    config: &Config,
    project: &Project,
    serialize_modules: &BTreeSet<String>,
    definitions: &[ViewDefinition],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    Ok(emit(
        config,
        project,
        serialize_modules,
        Some(definitions),
        None,
        false,
    )?)
}

pub fn emit_project_with_binding_visibility(
    config: &Config,
    project: &Project,
    serialize_modules: &BTreeSet<String>,
    definitions: &[ViewDefinition],
    public_bindings: Option<&std::collections::BTreeMap<String, BTreeSet<String>>>,
    statement_labels: bool,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    Ok(emit(
        config,
        project,
        serialize_modules,
        Some(definitions),
        public_bindings,
        statement_labels,
    )?)
}

fn emit(
    config: &Config,
    project: &Project,
    serialize_modules: &BTreeSet<String>,
    definitions: Option<&[ViewDefinition]>,
    public_bindings: Option<&std::collections::BTreeMap<String, BTreeSet<String>>>,
    statement_labels: bool,
) -> Result<()> {
    let plan = plan(
        config,
        project,
        serialize_modules,
        definitions,
        public_bindings,
        statement_labels,
    )?;
    if config.check {
        // Check mode only reads: it stages nothing and touches no path.
        let changes = plan.changes().map_err(|source| Error::ReadFile {
            path: config.output.clone(),
            source,
        })?;
        let desired = plan.desired.keys().cloned().collect();
        let problems = crate::check::problems(config, project, &changes, &desired)?;
        return if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::GeneratedCheckFailed { problems })
        };
    }
    let source_root =
        std::path::absolute(&config.source_root).map_err(|source| Error::CreateDir {
            path: config.source_root.clone(),
            source,
        })?;
    let stage_parent = source_root.parent().unwrap_or(&source_root);
    publication::publish(&plan, stage_parent).map_err(|source| Error::WriteFile {
        path: config.source_root.clone(),
        source: std::io::Error::other(source.to_string()),
    })
}

fn views_path(config: &Config) -> PathBuf {
    config.output.join(views::GENERATED_FILE)
}

fn plan(
    config: &Config,
    project: &Project,
    serialize_modules: &BTreeSet<String>,
    definitions: Option<&[ViewDefinition]>,
    public_bindings: Option<&std::collections::BTreeMap<String, BTreeSet<String>>>,
    statement_labels: bool,
) -> Result<Plan> {
    let mut plan = match config.target {
        Target::Rust => Plan {
            desired: rust::render_project(
                config,
                project,
                serialize_modules,
                public_bindings,
                statement_labels,
            )?,
            managed: managed_files(config)?,
            prune_root: Some(config.output.clone()),
        },
    };
    if let Some(definitions) = definitions {
        let path = views_path(config);
        plan.managed.insert(path.clone());
        if let Some(content) = views::render_generated_sql(definitions) {
            plan.desired.insert(path, content);
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TemporalConfig;
    use crate::model::{ConnectionAccess, Parameter, Query, ReturnType, ValueType};
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn nested_sql_owner_bindings_are_not_parent_prune_candidates() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("other_database");
        fs::create_dir(&nested).unwrap();
        fs::write(root.path().join("outer.generated.rs"), "").unwrap();
        fs::write(nested.join("marmot.toml"), "[tools.marmot]").unwrap();
        fs::write(nested.join("inner.generated.rs"), "retained child binding").unwrap();
        let config = Config {
            database: root.path().join("unused.db"),
            source_root: root.path().to_owned(),
            output: root.path().join("generated/sql"),
            target: Target::Rust,
            check: false,
            temporal: TemporalConfig::default(),
        };
        assert_eq!(
            managed_files(&config).unwrap(),
            BTreeSet::from([root.path().join("outer.generated.rs")])
        );
        assert_eq!(
            fs::read_to_string(nested.join("inner.generated.rs")).unwrap(),
            "retained child binding"
        );
    }

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

    fn query(root: &Path, module: &str, sql: &str, temporal: bool) -> Query {
        Query {
            source_path: root.join(format!("{module}.sql")),
            module_name: module.to_string(),
            name: format!("run_{}", module.replace('/', "_")),
            return_type: ReturnType::Execute,
            emit_batch: false,
            scalar_cardinality: crate::model::ScalarCardinality::Many,
            connection_access: ConnectionAccess::Mutation,
            sql: sql.to_string(),
            parameters: if temporal {
                vec![Parameter {
                    name: "day".to_string(),
                    sql_names: vec!["@day".to_string()],
                    column_type: ValueType::DbDate,
                    nullable: false,
                }]
            } else {
                vec![]
            },
            column_substitution: None,
            columns: vec![],
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        config: Config,
    }

    fn fixture(supplied_temporal: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("src");
        let output = root.join("generated/sql");
        fs::create_dir_all(&output).unwrap();
        let temporal = if supplied_temporal {
            TemporalConfig {
                module: Some("crate::calendar_support::temporal".to_string()),
                ..Default::default()
            }
        } else {
            TemporalConfig {
                module: Some("crate::generated::sql::temporal".to_string()),
                output: Some(output.join("temporal.rs")),
                ..Default::default()
            }
        };
        let config = Config {
            database: dir.path().join("unused.sqlite"),
            source_root: root.clone(),
            output,
            target: Target::Rust,
            check: false,
            temporal,
        };
        Fixture {
            _dir: dir,
            root,
            config,
        }
    }

    fn project(fixture: &Fixture, sql: &str) -> Project {
        Project {
            queries: vec![
                query(&fixture.root, "users", "update users set a = 1", false),
                query(&fixture.root, "events/days", sql, true),
            ],
        }
    }

    fn stamp_everything(fixture: &Fixture) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(&fixture.root) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                set_old(entry.path());
                files.push((entry.path().to_path_buf(), fs::read(entry.path()).unwrap()));
            }
        }
        files
    }

    fn assert_untouched(files: &[(PathBuf, Vec<u8>)]) {
        for (path, bytes) in files {
            assert_eq!(&fs::read(path).unwrap(), bytes, "{}", path.display());
            assert_eq!(modified(path), old_time(), "{}", path.display());
        }
    }

    #[test]
    fn writes_adjacent_bindings_and_a_fixed_shared_temporal_declaration() {
        let fixture = fixture(false);
        let project = project(&fixture, "update days set day = @day");
        let definitions = vec![ViewDefinition {
            name: "view_example".into(),
            columns: vec!["id".into()],
            create_sql: "CREATE VIEW view_example (id) AS SELECT 1".into(),
            source_path: fixture.root.join("view_example.sql"),
        }];
        emit_project_with_views(&fixture.config, &project, &BTreeSet::new(), &definitions).unwrap();

        let users = fs::read_to_string(fixture.root.join("users.generated.rs")).unwrap();
        assert!(users.starts_with("// @generated by Marmot from users.sql."));
        let days = fs::read_to_string(fixture.root.join("events/days.generated.rs")).unwrap();
        assert!(days.contains("use crate::generated::sql::temporal;"));
        let output = &fixture.config.output;
        assert_eq!(
            fs::read_to_string(output.join("mod.rs")).unwrap(),
            "pub mod temporal;\n"
        );
        assert!(output.join("temporal.rs").is_file());
        let mut names = fs::read_dir(output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            ["mod.rs", "temporal.rs", "views.sql"],
            "no central tree"
        );

        let before = stamp_everything(&fixture);
        emit_project_with_views(&fixture.config, &project, &BTreeSet::new(), &definitions).unwrap();
        assert_untouched(&before);
    }

    #[test]
    fn the_temporal_module_is_rustfmt_stable_and_survives_spelled_out_paths() {
        let mut fixture = fixture(false);
        let spelled = fixture.root.join("generated/./sql");
        fixture.config.output = spelled;
        let project = project(&fixture, "update days set day = @day");
        for _ in 0..2 {
            emit_project(&fixture.config, &project).unwrap();
        }
        let temporal = fixture.root.join("generated/sql/temporal.rs");
        assert!(
            temporal.is_file(),
            "a second run must not delete the module"
        );
        let formatted = std::process::Command::new("rustfmt")
            .args(["--edition", "2024", "--check"])
            .arg(&temporal)
            .output()
            .unwrap();
        assert!(
            formatted.status.success(),
            "{}",
            String::from_utf8_lossy(&formatted.stdout)
        );
    }

    #[test]
    fn a_target_without_temporal_queries_ignores_another_owners_temporal_folder() {
        let mut fixture = fixture(false);
        fixture.config.output = fixture.root.join("other/generated");
        let project = Project {
            queries: vec![query(
                &fixture.root,
                "users",
                "update users set a = 1",
                false,
            )],
        };
        emit_project(&fixture.config, &project).unwrap();
        assert!(!fixture.root.join("generated/sql/temporal.rs").exists());
        let error = emit_project(
            &fixture.config,
            &self::project(&fixture, "update d set a = @day"),
        )
        .unwrap_err();
        assert!(matches!(error, Error::InvalidTemporalConfig { .. }));
    }

    #[test]
    fn project_supplied_temporal_emits_no_module_and_no_declaration() {
        let fixture = fixture(true);
        let project = project(&fixture, "update days set day = @day");
        emit_project(&fixture.config, &project).unwrap();

        let days = fs::read_to_string(fixture.root.join("events/days.generated.rs")).unwrap();
        assert!(days.contains("use crate::calendar_support::temporal;"));
        let output = &fixture.config.output;
        assert!(!output.join("mod.rs").exists());
        assert!(!output.join("temporal.rs").exists());
    }

    #[test]
    fn regeneration_updates_only_affected_files() {
        let fixture = fixture(false);
        emit_project(
            &fixture.config,
            &project(&fixture, "update days set day = @day"),
        )
        .unwrap();
        let before = stamp_everything(&fixture);
        emit_project(
            &fixture.config,
            &project(&fixture, "update days set day = @day where active = 1"),
        )
        .unwrap();
        for (path, bytes) in &before {
            let changed = path.ends_with("events/days.generated.rs");
            assert_eq!(
                &fs::read(path).unwrap() != bytes,
                changed,
                "{}",
                path.display()
            );
            assert_eq!(modified(path) != old_time(), changed, "{}", path.display());
        }
    }

    #[test]
    fn check_reports_missing_stale_and_current_without_touching_anything() {
        let fixture = fixture(false);
        let project = project(&fixture, "update days set day = @day");
        let mut check = fixture.config.clone();
        check.check = true;
        let declare = |text: &str, file: &str| fs::write(fixture.root.join(file), text).unwrap();

        let missing = emit_project(&check, &project).unwrap_err();
        assert!(
            matches!(missing, Error::GeneratedCheckFailed { .. }),
            "{missing}"
        );
        assert_eq!(
            fs::read_dir(&fixture.root).unwrap().count(),
            1,
            "only generated/"
        );
        assert_eq!(fs::read_dir(&fixture.config.output).unwrap().count(), 0);

        emit_project(&fixture.config, &project).unwrap();
        declare("#[path = \"users.generated.rs\"]\nmod sql;\n", "users.rs");
        declare(
            "#[path = \"days.generated.rs\"]\nmod sql;\n",
            "events/days.rs",
        );
        let current = stamp_everything(&fixture);
        emit_project(&check, &project).unwrap();
        assert_untouched(&current);

        fs::write(fixture.root.join("users.generated.rs"), "stale").unwrap();
        let stale = stamp_everything(&fixture);
        let error = emit_project(&check, &project).unwrap_err();
        assert!(
            error.to_string().contains("generated file is stale"),
            "{error}"
        );
        assert_untouched(&stale);
        assert_eq!(
            fs::read_dir(fixture.root.parent().unwrap())
                .unwrap()
                .count(),
            1,
            "no staging folder is left beside the source root"
        );
    }

    #[test]
    fn generation_removes_orphans_and_a_retired_central_tree() {
        let fixture = fixture(false);
        let project = project(&fixture, "update days set day = @day");
        let central = fixture.config.output.join("pages/admin");
        fs::create_dir_all(&central).unwrap();
        fs::write(central.join("create.rs"), "pub mod x;").unwrap();
        fs::write(fixture.config.output.join("notes.txt"), "keep me").unwrap();
        fs::write(fixture.root.join("gone.generated.rs"), "orphan").unwrap();
        fs::write(fixture.root.join("handwritten.rs"), "keep me").unwrap();

        emit_project(&fixture.config, &project).unwrap();

        assert!(!fixture.root.join("gone.generated.rs").exists());
        assert!(!fixture.config.output.join("pages").exists());
        assert!(fixture.config.output.join("notes.txt").exists());
        assert!(fixture.root.join("handwritten.rs").exists());
    }

    #[test]
    fn temporal_use_requires_a_configured_module() {
        let mut fixture = fixture(false);
        fixture.config.temporal = TemporalConfig::default();
        let error =
            emit_project(&fixture.config, &project(&fixture, "update d set a = @day")).unwrap_err();
        assert!(matches!(error, Error::TemporalModuleNotConfigured));
        fixture.config.temporal.module = Some("temporal".to_string());
        let error =
            emit_project(&fixture.config, &project(&fixture, "update d set a = @day")).unwrap_err();
        assert!(matches!(error, Error::InvalidTemporalConfig { .. }));
    }

    #[test]
    fn failed_view_emission_preserves_bindings_until_recovery() {
        let fixture = fixture(false);
        let project = project(&fixture, "update days set day = @day");
        emit_project(&fixture.config, &project).unwrap();
        let obstruction = fixture.config.output.join("views.sql");
        fs::create_dir_all(&obstruction).unwrap();
        fs::write(obstruction.join("keep"), "keep this directory").unwrap();
        fs::write(fixture.root.join("gone.generated.rs"), "stale bindings").unwrap();
        let definitions = vec![ViewDefinition {
            name: "view_example".into(),
            columns: vec!["id".into()],
            create_sql: "CREATE VIEW view_example (id) AS SELECT 1".into(),
            source_path: fixture.root.join("view_example.sql"),
        }];
        let before = fs::read(fixture.root.join("users.generated.rs")).unwrap();
        assert!(
            emit_project_with_views(&fixture.config, &project, &BTreeSet::new(), &definitions)
                .is_err()
        );
        assert_eq!(
            fs::read(fixture.root.join("users.generated.rs")).unwrap(),
            before
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("gone.generated.rs")).unwrap(),
            "stale bindings"
        );
        fs::remove_dir_all(&obstruction).unwrap();
        emit_project_with_views(&fixture.config, &project, &BTreeSet::new(), &definitions).unwrap();
        assert!(!fixture.root.join("gone.generated.rs").exists());
        assert!(
            fs::read_to_string(&obstruction)
                .unwrap()
                .contains("CREATE VIEW view_example")
        );
    }
}
