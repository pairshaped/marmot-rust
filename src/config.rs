use std::collections::{BTreeMap, BTreeSet};
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
    pub output: PathBuf,
    pub target: Target,
    pub check: bool,
    pub temporal: TemporalConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileConfig {
    pub database: Option<PathBuf>,
    pub source_root: Option<PathBuf>,
    pub view_source_root: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub init_sql: Option<PathBuf>,
    pub migrations_dir: Option<PathBuf>,
    pub bootstrap_dir: Option<PathBuf>,
    pub seeds_dir: Option<PathBuf>,
    pub migration_table: Option<String>,
    pub schema_output: Option<PathBuf>,
    pub databases: BTreeMap<String, DatabaseReference>,
    pub serialize_modules: BTreeSet<String>,
    /// When configured, generated bindings are crate-visible unless listed here.
    pub public_bindings: Option<BTreeMap<String, BTreeSet<String>>>,
    pub temporal: TemporalConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalConfig {
    pub strict_suffixes: bool,
    pub datetime_suffixes: Vec<String>,
    pub date_suffixes: Vec<String>,
    pub datetime_storage: TemporalDateTimeStorage,
    pub date_storage: TemporalDateStorage,
    /// Absolute path of the module generated bindings import temporal types from.
    pub module: Option<String>,
    /// File Marmot writes the temporal module to, directly inside the shared
    /// output folder. When omitted, the project supplies the module.
    pub output: Option<PathBuf>,
    /// Emit the configured module even when the owner has no temporal queries.
    pub emit_without_queries: bool,
}

impl Default for TemporalConfig {
    fn default() -> Self {
        Self {
            strict_suffixes: false,
            datetime_suffixes: vec!["_at".to_string()],
            date_suffixes: vec!["_on".to_string()],
            datetime_storage: TemporalDateTimeStorage::TextSecondUtc,
            date_storage: TemporalDateStorage::TextYmd,
            module: None,
            output: None,
            emit_without_queries: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalDateTimeStorage {
    TextSecondUtc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalDateStorage {
    TextYmd,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseReference {
    pub path: Option<PathBuf>,
    pub source_root: Option<PathBuf>,
    pub migrations_dir: Option<PathBuf>,
    pub bootstrap_dir: Option<PathBuf>,
    pub seeds_dir: Option<PathBuf>,
    pub migration_table: Option<String>,
    pub schema_output: Option<PathBuf>,
    pub init_sql: Option<PathBuf>,
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

    #[error("malformed [tools.marmot.databases] entry")]
    MalformedDatabaseReference,

    #[error("unknown temporal datetime_storage {0}")]
    UnknownTemporalDateTimeStorage(String),

    #[error("unknown temporal date_storage {0}")]
    UnknownTemporalDateStorage(String),

    #[error("invalid temporal config: {0}")]
    InvalidTemporalConfig(String),

    #[error("invalid public_bindings config: {0}")]
    InvalidPublicBindings(String),

    #[error(
        "output {output} is the source root; it must be a folder below it, because Marmot removes stale generated Rust from the shared output folder"
    )]
    OutputIsSourceRoot { output: PathBuf },

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
            view_source_root: toml_path(marmot, "view_source_root"),
            output: toml_path(marmot, "output"),
            init_sql: toml_path(marmot, "init_sql"),
            migrations_dir: toml_path(marmot, "migrations_dir"),
            bootstrap_dir: toml_path(marmot, "bootstrap_dir"),
            seeds_dir: toml_path(marmot, "seeds_dir"),
            migration_table: toml_string(marmot, "migration_table"),
            schema_output: toml_path(marmot, "schema_output"),
            databases: toml_database_references(marmot)?,
            serialize_modules: marmot
                .and_then(|table| toml_string_array(table, "serialize_modules"))
                .unwrap_or_default()
                .into_iter()
                .collect(),
            temporal: toml_temporal_config(marmot)?,
            public_bindings: public_bindings(marmot)?,
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

fn public_bindings(
    marmot: Option<&toml::Value>,
) -> Result<Option<BTreeMap<String, BTreeSet<String>>>, ConfigError> {
    let Some(value) = marmot.and_then(|table| table.get("public_bindings")) else {
        return Ok(None);
    };
    let table = value
        .as_table()
        .ok_or_else(|| ConfigError::InvalidPublicBindings("expected a module table".into()))?;
    let mut bindings = BTreeMap::new();
    for (module, value) in table {
        let array = value.as_array().ok_or_else(|| {
            ConfigError::InvalidPublicBindings(format!("{module}: expected an array"))
        })?;
        let mut names = BTreeSet::new();
        for value in array {
            let name = value
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ConfigError::InvalidPublicBindings(format!(
                        "{module}: expected a nonempty item name"
                    ))
                })?;
            if !names.insert(name.to_owned()) {
                return Err(ConfigError::InvalidPublicBindings(format!(
                    "{module}: duplicate {name}"
                )));
            }
        }
        bindings.insert(module.clone(), names);
    }
    Ok(Some(bindings))
}

fn toml_path(table: Option<&toml::Value>, key: &str) -> Option<PathBuf> {
    let value = table?.get(key)?.as_str()?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn toml_string(table: Option<&toml::Value>, key: &str) -> Option<String> {
    table?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn toml_temporal_config(marmot: Option<&toml::Value>) -> Result<TemporalConfig, ConfigError> {
    let Some(temporal) = marmot.and_then(|marmot| marmot.get("temporal")) else {
        return Ok(TemporalConfig::default());
    };

    let mut config = TemporalConfig {
        module: toml_string(Some(temporal), "module"),
        output: toml_path(Some(temporal), "output"),
        ..Default::default()
    };
    if let Some(value) = temporal.get("emit_without_queries") {
        config.emit_without_queries = value.as_bool().ok_or_else(|| {
            ConfigError::InvalidTemporalConfig("emit_without_queries must be a boolean".into())
        })?;
    }
    if config.emit_without_queries && (config.module.is_none() || config.output.is_none()) {
        return Err(ConfigError::InvalidTemporalConfig(
            "emit_without_queries requires module and output".into(),
        ));
    }
    config.strict_suffixes = temporal
        .get("strict_suffixes")
        .and_then(|value| value.as_bool())
        .unwrap_or(config.strict_suffixes);
    config.datetime_suffixes =
        toml_string_array(temporal, "datetime_suffixes").unwrap_or(config.datetime_suffixes);
    config.date_suffixes =
        toml_string_array(temporal, "date_suffixes").unwrap_or(config.date_suffixes);
    if let Some(value) = temporal
        .get("datetime_storage")
        .and_then(|value| value.as_str())
    {
        config.datetime_storage = match value {
            "text_second_utc" => TemporalDateTimeStorage::TextSecondUtc,
            other => {
                return Err(ConfigError::UnknownTemporalDateTimeStorage(
                    other.to_string(),
                ));
            }
        };
    }
    if let Some(value) = temporal
        .get("date_storage")
        .and_then(|value| value.as_str())
    {
        config.date_storage = match value {
            "text_ymd" => TemporalDateStorage::TextYmd,
            other => return Err(ConfigError::UnknownTemporalDateStorage(other.to_string())),
        };
    }

    Ok(config)
}

fn toml_string_array(table: &toml::Value, key: &str) -> Option<Vec<String>> {
    let values = table.get(key)?.as_array()?;
    Some(
        values
            .iter()
            .filter_map(|value| value.as_str())
            .map(ToString::to_string)
            .collect(),
    )
}

fn toml_database_references(
    marmot: Option<&toml::Value>,
) -> Result<BTreeMap<String, DatabaseReference>, ConfigError> {
    let Some(databases) = marmot.and_then(|marmot| marmot.get("databases")) else {
        return Ok(BTreeMap::new());
    };

    if let Some(table) = databases.as_table() {
        let mut references = BTreeMap::new();
        for (name, value) in table {
            let Some(table) = value.as_table() else {
                return Err(ConfigError::MalformedDatabaseReference);
            };
            references.insert(name.clone(), database_reference_from_table(table));
        }
        Ok(references)
    } else if let Some(array) = databases.as_array() {
        let mut references = BTreeMap::new();
        for value in array {
            let Some(table) = value.as_table() else {
                return Err(ConfigError::MalformedDatabaseArrayEntry);
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
        Err(ConfigError::MalformedDatabaseReference)
    }
}

fn database_reference_from_table(table: &toml::map::Map<String, toml::Value>) -> DatabaseReference {
    DatabaseReference {
        path: table_path(table, "path"),
        source_root: table_path(table, "source_root"),
        migrations_dir: table_path(table, "migrations_dir"),
        bootstrap_dir: table_path(table, "bootstrap_dir"),
        seeds_dir: table_path(table, "seeds_dir"),
        migration_table: table_string(table, "migration_table"),
        schema_output: table_path(table, "schema_output"),
        init_sql: table_path(table, "init_sql"),
        output: table_path(table, "output"),
    }
}

fn table_path(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<PathBuf> {
    let value = table.get(key)?.as_str()?;
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn table_string(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<String> {
    table
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_bindings_distinguishes_default_and_explicit_owner_visibility() {
        assert!(
            FileConfig::from_toml_str("")
                .unwrap()
                .public_bindings
                .is_none()
        );
        let empty = FileConfig::from_toml_str("[tools.marmot.public_bindings]").unwrap();
        assert_eq!(empty.public_bindings, Some(BTreeMap::new()));
        let selected = FileConfig::from_toml_str(
            "[tools.marmot.public_bindings]\norders = [\"read\", \"Row::id\"]",
        )
        .unwrap();
        assert_eq!(
            selected.public_bindings.unwrap()["orders"],
            BTreeSet::from(["read".into(), "Row::id".into()])
        );
    }

    #[test]
    fn public_bindings_refuses_malformed_or_duplicate_declarations() {
        for source in [
            "[tools.marmot]\npublic_bindings = 1",
            "[tools.marmot.public_bindings]\norders = [1]",
            "[tools.marmot.public_bindings]\norders = [\"read\", \"read\"]",
        ] {
            assert!(matches!(
                FileConfig::from_toml_str(source),
                Err(ConfigError::InvalidPublicBindings(_))
            ));
        }
    }

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
            output = "src/generated"
            init_sql = "db/marmot_init.sql"
            migrations_dir = "db/migrations/app"
            bootstrap_dir = "db/bootstrap"
            seeds_dir = "db/seeds/app"
            migration_table = "schema_versions"
            schema_output = "db/schema.sql"
            "#,
        )
        .unwrap();

        assert_eq!(config.database, Some(PathBuf::from("dev.sqlite")));
        assert_eq!(config.source_root, Some(PathBuf::from("src")));
        assert_eq!(config.output, Some(PathBuf::from("src/generated")));
        assert_eq!(config.init_sql, Some(PathBuf::from("db/marmot_init.sql")));
        assert_eq!(
            config.migrations_dir,
            Some(PathBuf::from("db/migrations/app"))
        );
        assert_eq!(config.bootstrap_dir, Some(PathBuf::from("db/bootstrap")));
        assert_eq!(config.seeds_dir, Some(PathBuf::from("db/seeds/app")));
        assert_eq!(config.migration_table.as_deref(), Some("schema_versions"));
        assert_eq!(config.schema_output, Some(PathBuf::from("db/schema.sql")));
    }

    #[test]
    fn parses_opt_in_serializable_modules() {
        let config = FileConfig::from_toml_str(
            r#"
            [tools.marmot]
            serialize_modules = ["contacts", "orders/history"]
            "#,
        )
        .unwrap();

        assert_eq!(
            config.serialize_modules,
            BTreeSet::from(["contacts".to_string(), "orders/history".to_string()])
        );
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
    fn parses_temporal_config() {
        let config = FileConfig::from_toml_str(
            r#"
            [tools.marmot.temporal]
            strict_suffixes = true
            datetime_suffixes = ["_at", "_time"]
            date_suffixes = ["_on", "_date"]
            datetime_storage = "text_second_utc"
            date_storage = "text_ymd"
            module = "crate::generated::sql::temporal"
            output = "src/generated/sql/temporal.rs"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.temporal.module.as_deref(),
            Some("crate::generated::sql::temporal")
        );
        assert_eq!(
            config.temporal.output,
            Some(PathBuf::from("src/generated/sql/temporal.rs"))
        );
        assert!(config.temporal.strict_suffixes);
        assert_eq!(config.temporal.datetime_suffixes, ["_at", "_time"]);
        assert_eq!(config.temporal.date_suffixes, ["_on", "_date"]);
        assert_eq!(
            config.temporal.datetime_storage,
            TemporalDateTimeStorage::TextSecondUtc
        );
        assert_eq!(config.temporal.date_storage, TemporalDateStorage::TextYmd);
    }

    #[test]
    fn parses_named_database_tables() {
        let config = FileConfig::from_toml_str(
            r#"
            [tools.marmot.databases.app]
            path = "db/app.db"
            source_root = "src/app"
            migrations_dir = "db/migrations/app"
            bootstrap_dir = "db/bootstrap/app"
            migration_table = "app_schema_versions"
            schema_output = "db/app_schema.sql"
            init_sql = "db/app_init.sql"

            [tools.marmot.databases.analytics]
            path = "db/analytics.db"
            output = "src/generated/sql/analytics"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.databases["app"].path,
            Some(PathBuf::from("db/app.db"))
        );
        assert_eq!(
            config.databases["app"].source_root,
            Some(PathBuf::from("src/app"))
        );
        assert_eq!(
            config.databases["app"].migrations_dir,
            Some(PathBuf::from("db/migrations/app"))
        );
        assert_eq!(
            config.databases["app"].init_sql,
            Some(PathBuf::from("db/app_init.sql"))
        );
        assert_eq!(
            config.databases["app"].schema_output,
            Some(PathBuf::from("db/app_schema.sql"))
        );
        assert_eq!(
            config.databases["app"].bootstrap_dir,
            Some(PathBuf::from("db/bootstrap/app"))
        );
        assert_eq!(
            config.databases["app"].migration_table.as_deref(),
            Some("app_schema_versions")
        );
        assert_eq!(
            config.databases["analytics"].path,
            Some(PathBuf::from("db/analytics.db"))
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

    #[test]
    fn rejects_named_database_array_entry_with_empty_name() {
        let error = FileConfig::from_toml_str(
            r#"
            [[tools.marmot.databases]]
            name = ""
            path = "db/primary.sqlite"
            "#,
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::MalformedDatabaseArrayEntry));
    }

    #[test]
    fn rejects_named_database_table_entry_that_is_not_a_table() {
        let error = FileConfig::from_toml_str(
            r#"
            [tools.marmot.databases]
            app = "db/app.sqlite"
            "#,
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::MalformedDatabaseReference));
    }

    #[test]
    fn rejects_named_database_array_value_that_is_not_a_table() {
        let error = FileConfig::from_toml_str(
            r#"
            [tools.marmot]
            databases = [
              { name = "primary", path = "db/primary.sqlite" },
              "db/analytics.sqlite",
            ]
            "#,
        )
        .unwrap_err();

        assert!(matches!(error, ConfigError::MalformedDatabaseArrayEntry));
    }
}
