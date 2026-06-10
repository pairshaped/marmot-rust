pub mod analyzer;
pub mod config;
pub mod discovery;
pub mod emit;
pub mod error;
pub mod migrations;
pub mod model;
pub mod reset;
pub mod seeds;
mod sql_files;
pub mod sql_text;
pub mod sqlite;

pub use analyzer::analyze_project;
pub use config::{Config, Target};
pub use discovery::{discover_sql_files, discover_sql_files_with_sql_dir};
pub use emit::emit_project;
pub use error::{Error, Result};
