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
