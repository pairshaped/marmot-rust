mod rust;

use crate::config::{Config, Target};
use crate::error::Result;
use crate::model::Project;

pub fn emit_project(config: &Config, project: &Project) -> Result<()> {
    match config.target {
        Target::Rust => rust::emit(config, project),
    }
}
