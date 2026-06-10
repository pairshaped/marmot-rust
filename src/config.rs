use std::fs;
use std::path::{Path, PathBuf};

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileConfig {
    pub database: Option<PathBuf>,
    pub source_root: Option<PathBuf>,
    pub sql_dir: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub migrations_dir: Option<PathBuf>,
    pub seeds_dir: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not parse config {path}: {source}")]
    ParseToml {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error(
        "missing required database path; pass --database, set DATABASE_URL, or configure [tools.marmot].database"
    )]
    MissingDatabase,
}

impl FileConfig {
    pub fn from_toml_str(value: &str) -> Result<Self, toml::de::Error> {
        let parsed = value.parse::<toml::Value>()?;
        let marmot = parsed.get("tools").and_then(|tools| tools.get("marmot"));

        Ok(Self {
            database: toml_path(marmot, "database"),
            source_root: toml_path(marmot, "source_root"),
            sql_dir: toml_path(marmot, "sql_dir"),
            output: toml_path(marmot, "output"),
            migrations_dir: toml_path(marmot, "migrations_dir"),
            seeds_dir: toml_path(marmot, "seeds_dir"),
        })
    }

    pub fn read_optional(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(value) => Self::from_toml_str(&value).map_err(|source| ConfigError::ParseToml {
                path: path.to_path_buf(),
                source,
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::ReadFile {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

fn toml_path(table: Option<&toml::Value>, key: &str) -> Option<PathBuf> {
    let value = table?.get(key)?.as_str()?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_toml_as_empty_config() {
        assert_eq!(
            FileConfig::from_toml_str("").unwrap(),
            FileConfig::default()
        );
    }

    #[test]
    fn parses_tools_marmot_paths_from_toml() {
        let config = FileConfig::from_toml_str(
            r#"
            [tools.marmot]
            database = "dev.sqlite"
            source_root = "src"
            sql_dir = "src/sql"
            output = "src/generated"
            migrations_dir = "db/migrations/app"
            seeds_dir = "db/seeds/app"
            "#,
        )
        .unwrap();

        assert_eq!(config.database, Some(PathBuf::from("dev.sqlite")));
        assert_eq!(config.source_root, Some(PathBuf::from("src")));
        assert_eq!(config.sql_dir, Some(PathBuf::from("src/sql")));
        assert_eq!(config.output, Some(PathBuf::from("src/generated")));
        assert_eq!(
            config.migrations_dir,
            Some(PathBuf::from("db/migrations/app"))
        );
        assert_eq!(config.seeds_dir, Some(PathBuf::from("db/seeds/app")));
    }

    #[test]
    fn ignores_empty_path_values() {
        let config = FileConfig::from_toml_str(
            r#"
            [tools.marmot]
            database = ""
            output = ""
            "#,
        )
        .unwrap();

        assert_eq!(config.database, None);
        assert_eq!(config.output, None);
    }
}
