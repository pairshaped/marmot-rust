mod rust;

use std::collections::BTreeSet;

use crate::config::{Config, Target};
use crate::error::Result;
use crate::model::Project;

pub fn emit_project(config: &Config, project: &Project) -> Result<()> {
    emit_project_with_serialize_modules(config, project, &BTreeSet::new())
}

pub fn emit_project_with_serialize_modules(
    config: &Config,
    project: &Project,
    serialize_modules: &BTreeSet<String>,
) -> Result<()> {
    match config.target {
        Target::Rust => rust::emit(config, project, serialize_modules),
    }
}

/// Publish one owner's bindings and view installation SQL as one complete tree.
/// No publication is attempted until both emitters have finished successfully.
pub fn emit_project_with_views(
    config: &Config,
    project: &Project,
    serialize_modules: &BTreeSet<String>,
    definitions: &[crate::views::ViewDefinition],
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    if config.check {
        rust::emit_staged(config, project, serialize_modules)?;
        crate::views::emit_generated_sql_staged(definitions, &config.output, true)?;
        return Ok(());
    }
    crate::publication::publish(&config.output, |output| {
        let mut staged_config = config.clone();
        staged_config.output = output.to_path_buf();
        rust::emit_staged(&staged_config, project, serialize_modules)?;
        crate::views::emit_generated_sql_staged(definitions, output, false)?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn failed_view_emission_preserves_bindings_and_stale_output_until_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("sql");
        fs::create_dir_all(output.join("views.sql")).unwrap();
        fs::write(output.join("views.sql/obstruction"), "keep this directory").unwrap();
        fs::write(output.join("mod.rs"), "old bindings").unwrap();
        fs::write(output.join("stale.rs"), "stale bindings").unwrap();
        let config = Config {
            database: temp.path().join("unused.sqlite"),
            source_root: temp.path().join("src"),
            output: output.clone(),
            target: Target::Rust,
            check: false,
            temporal: Default::default(),
        };
        let project = Project { queries: vec![] };
        let definitions = vec![crate::views::ViewDefinition {
            name: "view_example".into(),
            columns: vec!["id".into()],
            create_sql: "CREATE VIEW view_example (id) AS SELECT 1".into(),
            source_path: temp.path().join("view_example.sql"),
        }];
        assert!(
            emit_project_with_views(&config, &project, &BTreeSet::new(), &definitions).is_err()
        );
        assert_eq!(
            fs::read_to_string(output.join("mod.rs")).unwrap(),
            "old bindings"
        );
        assert_eq!(
            fs::read_to_string(output.join("stale.rs")).unwrap(),
            "stale bindings"
        );
        fs::remove_dir_all(output.join("views.sql")).unwrap();
        emit_project_with_views(&config, &project, &BTreeSet::new(), &definitions).unwrap();
        assert!(!output.join("stale.rs").exists());
        assert!(
            fs::read_to_string(output.join("views.sql"))
                .unwrap()
                .contains("CREATE VIEW view_example")
        );
        let mut check = config.clone();
        check.check = true;
        emit_project_with_views(&check, &project, &BTreeSet::new(), &definitions).unwrap();
    }
}
