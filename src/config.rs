use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    Rust,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database: PathBuf,
    pub source_root: PathBuf,
    pub sql_dir: Option<PathBuf>,
    pub output: PathBuf,
    pub target: Target,
    pub check: bool,
}
