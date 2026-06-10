use std::collections::BTreeMap;
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
    pub databases: BTreeMap<String, DatabaseReference>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseReference {
    pub path: Option<PathBuf>,
    pub migrations_dir: Option<PathBuf>,
    pub seeds_dir: Option<PathBuf>,
    pub sql_dir: Option<PathBuf>,
    pub output: Option<PathBuf>,
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

    #[error(
        "mixed database configuration: [tools.marmot].database cannot be used with [tools.marmot.databases]"
    )]
    MixedDatabaseConfig,

    #[error("unknown database name {0}")]
    UnknownDatabaseName(String),

    #[error("--database cannot be used with --database-name when named databases are configured")]
    MixedDatabaseCliArgs,

    #[error("missing or empty name in [[tools.marmot.databases]]")]
    MalformedDatabaseArrayEntry,

    #[error("output path must be under source root: output {output}, source root {source_root}")]
    OutputOutsideSourceRoot {
        output: PathBuf,
        source_root: PathBuf,
    },
}

impl FileConfig {
    pub fn from_toml_str(value: &str) -> Result<Self, ConfigError> {
        let parsed = value
            .parse::<toml::Value>()
            .map_err(|source| ConfigError::ParseToml {
                path: PathBuf::from("<inline>"),
                source,
            })?;
        let marmot = parsed.get("tools").and_then(|tools| tools.get("marmot"));

        Ok(Self {
            database: toml_path(marmot, "database"),
            source_root: toml_path(marmot, "source_root"),
            sql_dir: toml_path(marmot, "sql_dir"),
            output: toml_path(marmot, "output"),
            migrations_dir: toml_path(marmot, "migrations_dir"),
            seeds_dir: toml_path(marmot, "seeds_dir"),
            databases: toml_database_references(marmot)?,
        })
    }

    pub fn read_optional(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(value) => match Self::from_toml_str(&value) {
                Ok(config) => Ok(config),
                Err(ConfigError::ParseToml { source, .. }) => Err(ConfigError::ParseToml {
                    path: path.to_path_buf(),
                    source,
                }),
                Err(source) => Err(source),
            },
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

fn toml_database_references(
    marmot: Option<&toml::Value>,
) -> Result<BTreeMap<String, DatabaseReference>, ConfigError> {
    let Some(databases) = marmot.and_then(|marmot| marmot.get("databases")) else {
        return Ok(BTreeMap::new());
    };

    if let Some(table) = databases.as_table() {
        Ok(table
            .iter()
            .filter_map(|(name, value)| {
                let table = value.as_table()?;
                Some((name.clone(), database_reference_from_table(table)))
            })
            .collect())
    } else if let Some(array) = databases.as_array() {
        let mut references = BTreeMap::new();
        for value in array {
            let Some(table) = value.as_table() else {
                continue;
            };
            let Some(name) = table.get("name").and_then(|value| value.as_str()) else {
                return Err(ConfigError::MalformedDatabaseArrayEntry);
            };
            let name = name.trim();
            if name.is_empty() {
                return Err(ConfigError::MalformedDatabaseArrayEntry);
            }
            references.insert(name.to_string(), database_reference_from_table(table));
        }
        Ok(references)
    } else {
        Ok(BTreeMap::new())
    }
}

fn database_reference_from_table(table: &toml::map::Map<String, toml::Value>) -> DatabaseReference {
    DatabaseReference {
        path: table_path(table, "path"),
        migrations_dir: table_path(table, "migrations_dir"),
        seeds_dir: table_path(table, "seeds_dir"),
        sql_dir: table_path(table, "sql_dir"),
        output: table_path(table, "output"),
    }
}

fn table_path(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<PathBuf> {
    let value = table.get(key)?.as_str()?;
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

    #[test]
    fn parses_named_database_tables() {
        let config = FileConfig::from_toml_str(
            r#"
            [tools.marmot.databases.app]
            path = "db/app.db"
            migrations_dir = "db/migrations/app"

            [tools.marmot.databases.analytics]
            path = "db/analytics.db"
            sql_dir = "src/sql/analytics"
            output = "src/generated/sql/analytics"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.databases["app"].path,
            Some(PathBuf::from("db/app.db"))
        );
        assert_eq!(
            config.databases["app"].migrations_dir,
            Some(PathBuf::from("db/migrations/app"))
        );
        assert_eq!(
            config.databases["analytics"].path,
            Some(PathBuf::from("db/analytics.db"))
        );
        assert_eq!(
            config.databases["analytics"].sql_dir,
            Some(PathBuf::from("src/sql/analytics"))
        );
        assert_eq!(
            config.databases["analytics"].output,
            Some(PathBuf::from("src/generated/sql/analytics"))
        );
    }

    #[test]
    fn parses_named_database_array_entries_by_name() {
        let config = FileConfig::from_toml_str(
            r#"
            [[tools.marmot.databases]]
            name = "primary"
            path = "db/primary.sqlite"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.databases["primary"].path,
            Some(PathBuf::from("db/primary.sqlite"))
        );
    }

    #[test]
    fn rejects_named_database_array_entry_without_name() {
        let error = FileConfig::from_toml_str(
            r#"
            [[tools.marmot.databases]]
            path = "db/primary.sqlite"
            "#,
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::MalformedDatabaseArrayEntry));
    }
}
