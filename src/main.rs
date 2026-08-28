use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use clap::{Parser, Subcommand};
use marmot::{
    Config, Error as MarmotError, FileConfig, Target, analyze_project_with_init_sql,
    config::{ConfigError, DatabaseReference},
    emit_project_with_serialize_modules, migrations,
    model::{Project, ValueType},
    reset, schema, seeds,
    validation::{self, IntegrityMode, ValidationConfig, ValidationOutput},
    views,
};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Generate typed database code from colocated SQL files"
)]
struct Cli {
    #[arg(long, global = true, default_value = "marmot.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect(Args),
    Generate(Args),
    Migrate(MigrateArgs),
    Bootstrap(BootstrapArgs),
    Seed(SeedArgs),
    Reset(ResetArgs),
    DumpSchema(DumpSchemaArgs),
    AuditViews(AuditViewsArgs),
    Validate(ValidateArgs),
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    source_root: Option<PathBuf>,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = Target::Rust)]
    target: Target,

    #[arg(long)]
    check: bool,
}

#[derive(Debug, Parser)]
struct MigrateArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    migrations_dir: Option<PathBuf>,

    #[arg(long)]
    source_root: Option<PathBuf>,

    #[arg(long)]
    deny_view_warnings: bool,

    #[arg(long)]
    migration_table: Option<String>,

    #[arg(long)]
    schema_output: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct SeedArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    seeds_dir: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct BootstrapArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    bootstrap_dir: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct ResetArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    migrations_dir: Option<PathBuf>,

    #[arg(long)]
    bootstrap_dir: Option<PathBuf>,

    #[arg(long)]
    seeds_dir: Option<PathBuf>,

    #[arg(long)]
    source_root: Option<PathBuf>,

    #[arg(long)]
    deny_view_warnings: bool,

    #[arg(long)]
    migration_table: Option<String>,

    #[arg(long)]
    schema_output: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct DumpSchemaArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long)]
    check: bool,
}

#[derive(Debug, Parser)]
struct AuditViewsArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    source_root: Option<PathBuf>,

    #[arg(long)]
    deny_warnings: bool,
}

#[derive(Debug, Parser)]
struct ValidateArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    migrations_dir: Option<PathBuf>,

    #[arg(long)]
    source_root: Option<PathBuf>,

    #[arg(long)]
    migration_table: Option<String>,

    #[arg(long)]
    full: bool,

    #[arg(long)]
    json: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let file_config = FileConfig::read_optional(&cli.config)?;
    match cli.command {
        Command::Inspect(args) => {
            for target in configs(args, &file_config)? {
                let project =
                    analyze_project_with_init_sql(&target.config, target.init_sql.as_deref())?;
                let audit =
                    views::audit_database(&target.config.database, &target.config.source_root)?;
                print_view_warnings(&audit, &target.config.source_root);
                for query in project.queries {
                    println!(
                        "{}::{} params={} columns={} source={}",
                        query.module_name,
                        query.name,
                        query.parameters.len(),
                        query.columns.len(),
                        query.source_path.display()
                    );
                }
            }
        }
        Command::Generate(args) => {
            let mut analyzed = Vec::new();
            let configs = configs(args, &file_config)?;
            for target in &configs {
                validate_output_under_source_root(
                    &target.config.source_root,
                    &target.config.output,
                )?;
            }
            for target in configs {
                let project =
                    analyze_project_with_init_sql(&target.config, target.init_sql.as_deref())?;
                analyzed.push((target.config, project, target.serialize_modules));
            }
            ensure_generated_outputs_do_not_collide(&analyzed)?;
            for (config, project, serialize_modules) in analyzed {
                emit_project_with_serialize_modules(&config, &project, &serialize_modules)?;
                let definitions = views::discover(&config.source_root)?;
                views::emit_generated_sql(&definitions, &config.output, config.check)?;
                let audit = views::audit_database(&config.database, &config.source_root)?;
                if config.check {
                    audit.deny_warnings(&config.source_root)?;
                } else {
                    print_view_warnings(&audit, &config.source_root);
                }
            }
        }
        Command::Migrate(args) => {
            for target in database_targets(args.database, args.database_name, &file_config)? {
                let source_root =
                    resolved_source_root(&target, args.source_root.as_ref(), &file_config);
                let migrations_dir = args
                    .migrations_dir
                    .clone()
                    .or(target.migrations_dir.clone())
                    .unwrap_or_else(|| PathBuf::from(migrations::MIGRATION_DIR));
                let migration_table = resolved_migration_table(&args.migration_table, &target);
                let applied = migrations::migrate_from_with_tracking_table(
                    &target.database,
                    migrations_dir,
                    migration_table,
                )?;
                print_applied("Applied", &applied);
                let audit = views::reconcile_database(&target.database, &source_root)?;
                if args.deny_view_warnings {
                    audit.deny_warnings(&source_root)?;
                } else {
                    print_view_warnings(&audit, &source_root);
                }
                dump_schema_if_configured(
                    &target.database,
                    args.schema_output
                        .as_ref()
                        .or(target.schema_output.as_ref()),
                )?;
            }
        }
        Command::Seed(args) => {
            for target in database_targets(args.database, args.database_name, &file_config)? {
                let seeds_dir = args
                    .seeds_dir
                    .clone()
                    .or(target.seeds_dir)
                    .unwrap_or_else(|| PathBuf::from(seeds::SEED_DIR));
                let applied = seeds::seed_from(target.database, seeds_dir)?;
                print_applied("Ran", &applied);
            }
        }
        Command::Bootstrap(args) => {
            for target in database_targets(args.database, args.database_name, &file_config)? {
                let bootstrap_dir = args
                    .bootstrap_dir
                    .clone()
                    .or(target.bootstrap_dir)
                    .ok_or_else(|| {
                        std::io::Error::other(
                            "missing bootstrap directory; pass --bootstrap-dir or configure bootstrap_dir",
                        )
                    })?;
                let applied = seeds::seed_from(target.database, bootstrap_dir)?;
                print_applied("Ran", &applied);
            }
        }
        Command::Reset(args) => {
            for target in database_targets(args.database, args.database_name, &file_config)? {
                let source_root =
                    resolved_source_root(&target, args.source_root.as_ref(), &file_config);
                let migrations_dir = args
                    .migrations_dir
                    .clone()
                    .or(target.migrations_dir.clone())
                    .unwrap_or_else(|| PathBuf::from(migrations::MIGRATION_DIR));
                let migration_table = resolved_migration_table(&args.migration_table, &target);
                let bootstrap_dir = args.bootstrap_dir.clone().or(target.bootstrap_dir.clone());
                let seeds_dir = args
                    .seeds_dir
                    .clone()
                    .or(target.seeds_dir.clone())
                    .unwrap_or_else(|| PathBuf::from(seeds::SEED_DIR));
                let (applied_migrations, applied_seeds, audit) =
                    reset::reset_with_views_bootstrap_and_seeds_from(
                        &target.database,
                        migrations_dir,
                        bootstrap_dir,
                        seeds_dir,
                        &source_root,
                        migration_table,
                    )?;
                print_applied("Applied", &applied_migrations);
                print_applied("Ran", &applied_seeds);
                if args.deny_view_warnings {
                    audit.deny_warnings(&source_root)?;
                } else {
                    print_view_warnings(&audit, &source_root);
                }
                dump_schema_if_configured(
                    &target.database,
                    args.schema_output
                        .as_ref()
                        .or(target.schema_output.as_ref()),
                )?;
            }
        }
        Command::AuditViews(args) => {
            for target in database_targets(args.database, args.database_name, &file_config)? {
                let source_root =
                    resolved_source_root(&target, args.source_root.as_ref(), &file_config);
                let audit = views::audit_database(&target.database, &source_root)?;
                if args.deny_warnings {
                    audit.deny_warnings(&source_root)?;
                } else {
                    print_view_warnings(&audit, &source_root);
                }
            }
        }
        Command::DumpSchema(args) => {
            let targets = database_targets(args.database, args.database_name, &file_config)?;
            if targets.len() != 1 {
                return Err(std::io::Error::other(
                    "dump-schema requires one database; pass --database-name",
                )
                .into());
            }
            let output = args
                .output
                .or_else(|| targets[0].schema_output.clone())
                .unwrap_or_else(|| PathBuf::from("db/schema.sql"));
            let result = schema::dump(&targets[0].database, &output, args.check)?;
            if result == schema::DumpResult::Written {
                println!("Wrote {}", output.display());
            }
        }
        Command::Validate(args) => {
            let targets = database_targets(args.database, args.database_name, &file_config)?;
            let mut reports = Vec::new();
            for target in targets {
                let source_root =
                    resolved_source_root(&target, args.source_root.as_ref(), &file_config);
                let migrations_dir = args
                    .migrations_dir
                    .clone()
                    .or(target.migrations_dir.clone())
                    .unwrap_or_else(|| PathBuf::from(migrations::MIGRATION_DIR));
                let migration_table =
                    resolved_migration_table(&args.migration_table, &target).to_string();
                reports.push(validation::validate(&ValidationConfig {
                    database: target.database,
                    source_root,
                    migrations_dir,
                    migration_table,
                    integrity_mode: if args.full {
                        IntegrityMode::Full
                    } else {
                        IntegrityMode::Quick
                    },
                })?);
            }
            let output = ValidationOutput {
                format_version: 1,
                databases: reports,
            };
            if args.json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                print_validation_output(&output);
            }
            if !output.passed() {
                return Err(std::io::Error::other("database validation failed").into());
            }
        }
    }
    Ok(())
}

fn print_validation_output(output: &ValidationOutput) {
    for report in &output.databases {
        println!("Database: {}", report.database);
        for check in &report.checks {
            let status = if check.status == validation::CheckStatus::Passed {
                "passed"
            } else {
                "failed"
            };
            println!("  {}: {status}", check.name);
            for detail in &check.details {
                println!("    {detail}");
            }
        }
        println!("  SQLite: {}", report.runtime.sqlite_version);
        println!(
            "  Planner statistics: {} ({} rows)",
            if report.runtime.planner_statistics.present {
                "present"
            } else {
                "absent"
            },
            report.runtime.planner_statistics.rows
        );
        println!(
            "  Compile options: {}",
            report.runtime.compile_options.join(", ")
        );
    }
}

fn resolved_migration_table<'a>(cli: &'a Option<String>, target: &'a DatabaseTarget) -> &'a str {
    cli.as_deref()
        .or(target.migration_table.as_deref())
        .unwrap_or(migrations::TRACKING_TABLE)
}

fn dump_schema_if_configured(
    database: &Path,
    output: Option<&PathBuf>,
) -> Result<(), schema::SchemaError> {
    let Some(output) = output else {
        return Ok(());
    };
    if schema::dump(database, output, false)? == schema::DumpResult::Written {
        println!("Wrote {}", output.display());
    }
    Ok(())
}

fn print_applied(action: &str, applied: &[String]) {
    for version in applied {
        println!("{action} {version}");
    }
}

fn print_view_warnings(audit: &views::ViewAudit, source_root: &Path) {
    for warning in audit.warnings(source_root) {
        eprintln!("{warning}");
    }
}

fn ensure_generated_outputs_do_not_collide(
    analyzed: &[(Config, Project, BTreeSet<String>)],
) -> Result<(), MarmotError> {
    let mut by_path: BTreeMap<PathBuf, BTreeSet<usize>> = BTreeMap::new();
    for (target_index, (config, project, _)) in analyzed.iter().enumerate() {
        for path in generated_output_paths(config, project) {
            by_path.entry(path).or_default().insert(target_index);
        }
    }

    let collisions = by_path
        .into_iter()
        .filter_map(|(path, targets)| (targets.len() > 1).then_some(path))
        .collect::<Vec<_>>();

    if collisions.is_empty() {
        Ok(())
    } else {
        Err(MarmotError::GeneratedOutputCollision { paths: collisions })
    }
}

fn generated_output_paths(config: &Config, project: &Project) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    for query in &project.queries {
        paths.insert(generated_module_path(&config.output, &query.module_name));
        let mut prefix = config.output.clone();
        paths.insert(prefix.join("mod.rs"));
        let segments = query.module_name.split('/').collect::<Vec<_>>();
        for segment in segments.iter().take(segments.len().saturating_sub(1)) {
            prefix.push(segment);
            paths.insert(prefix.join("mod.rs"));
        }
    }
    paths.insert(config.output.join("mod.rs"));
    if project_uses_temporal(project) {
        paths.insert(config.output.join("temporal.rs"));
    }
    paths
}

fn project_uses_temporal(project: &Project) -> bool {
    project.queries.iter().any(|query| {
        query
            .parameters
            .iter()
            .any(|param| matches!(param.column_type, ValueType::DbDate | ValueType::DbDateTime))
            || query.columns.iter().any(|column| {
                matches!(
                    column.column_type,
                    ValueType::DbDate | ValueType::DbDateTime
                )
            })
    })
}

fn generated_module_path(output: &Path, module: &str) -> PathBuf {
    let mut path = output.to_path_buf();
    for segment in module.split('/') {
        path.push(segment);
    }
    path.set_extension("rs");
    path
}

#[derive(Debug)]
struct AnalysisTarget {
    config: Config,
    init_sql: Option<PathBuf>,
    serialize_modules: BTreeSet<String>,
}

fn configs(args: Args, file_config: &FileConfig) -> Result<Vec<AnalysisTarget>, ConfigError> {
    let targets = database_targets(args.database, args.database_name, file_config)?;
    let cli_source_root = args.source_root;
    let output = args.output;
    let target = args.target;
    let check = args.check;

    Ok(targets
        .into_iter()
        .map(|database_target| {
            let config_source_root =
                resolved_source_root(&database_target, cli_source_root.as_ref(), file_config);
            AnalysisTarget {
                config: Config {
                    database: database_target.database,
                    source_root: config_source_root.clone(),
                    output: output
                        .clone()
                        .or(database_target.output)
                        .unwrap_or_else(|| config_source_root.join("generated/sql")),
                    target,
                    check,
                    temporal: file_config.temporal.clone(),
                },
                init_sql: database_target.init_sql,
                serialize_modules: file_config.serialize_modules.clone(),
            }
        })
        .collect())
}

fn resolved_source_root(
    target: &DatabaseTarget,
    cli_source_root: Option<&PathBuf>,
    file_config: &FileConfig,
) -> PathBuf {
    if let Some(cli_source_root) = cli_source_root {
        target
            .source_root_namespace
            .as_deref()
            .map(|name| join_namespace(cli_source_root.clone(), name))
            .unwrap_or_else(|| cli_source_root.clone())
    } else {
        target
            .source_root
            .clone()
            .or_else(|| file_config.source_root.clone())
            .unwrap_or_else(|| PathBuf::from("src"))
    }
}

#[derive(Debug)]
struct DatabaseTarget {
    database: PathBuf,
    source_root: Option<PathBuf>,
    source_root_namespace: Option<String>,
    output: Option<PathBuf>,
    init_sql: Option<PathBuf>,
    migrations_dir: Option<PathBuf>,
    bootstrap_dir: Option<PathBuf>,
    seeds_dir: Option<PathBuf>,
    migration_table: Option<String>,
    schema_output: Option<PathBuf>,
}

fn database_targets(
    cli_database: Option<PathBuf>,
    cli_database_name: Option<String>,
    file_config: &FileConfig,
) -> Result<Vec<DatabaseTarget>, ConfigError> {
    if file_config.database.is_some() && !file_config.databases.is_empty() {
        return Err(ConfigError::MixedDatabaseConfig);
    }

    if !file_config.databases.is_empty() && cli_database.is_some() && cli_database_name.is_some() {
        return Err(ConfigError::MixedDatabaseCliArgs);
    }

    if let Some(name) = cli_database_name {
        if let Some(reference) = file_config.databases.get(&name) {
            return Ok(vec![named_database_target(&name, reference, file_config)]);
        }
        if file_config.databases.is_empty() {
            return simple_database_target(cli_database, file_config)
                .map(|target| vec![target])
                .map_err(|_| ConfigError::UnknownDatabaseName(name));
        }
        return Err(ConfigError::UnknownDatabaseName(name));
    }

    if !file_config.databases.is_empty() && cli_database.is_none() {
        return Ok(file_config
            .databases
            .iter()
            .map(|(name, reference)| named_database_target(name, reference, file_config))
            .collect());
    }

    if let Some(database) = explicit_database_path(cli_database, file_config) {
        return Ok(vec![DatabaseTarget {
            database,
            source_root: None,
            source_root_namespace: None,
            output: file_config.output.clone(),
            init_sql: file_config.init_sql.clone(),
            migrations_dir: file_config.migrations_dir.clone(),
            bootstrap_dir: file_config.bootstrap_dir.clone(),
            seeds_dir: file_config.seeds_dir.clone(),
            migration_table: file_config.migration_table.clone(),
            schema_output: file_config.schema_output.clone(),
        }]);
    }

    Err(ConfigError::MissingDatabase)
}

fn simple_database_target(
    cli_database: Option<PathBuf>,
    file_config: &FileConfig,
) -> Result<DatabaseTarget, ConfigError> {
    explicit_database_path(cli_database, file_config)
        .map(|database| DatabaseTarget {
            database,
            source_root: None,
            source_root_namespace: None,
            output: file_config.output.clone(),
            init_sql: file_config.init_sql.clone(),
            migrations_dir: file_config.migrations_dir.clone(),
            bootstrap_dir: file_config.bootstrap_dir.clone(),
            seeds_dir: file_config.seeds_dir.clone(),
            migration_table: file_config.migration_table.clone(),
            schema_output: file_config.schema_output.clone(),
        })
        .ok_or(ConfigError::MissingDatabase)
}

fn explicit_database_path(
    cli_database: Option<PathBuf>,
    file_config: &FileConfig,
) -> Option<PathBuf> {
    cli_database
        .or_else(|| std::env::var_os("DATABASE_URL").map(PathBuf::from))
        .or_else(|| file_config.database.clone())
        .filter(|path| !path.as_os_str().is_empty())
}

fn named_database_target(
    name: &str,
    reference: &DatabaseReference,
    file_config: &FileConfig,
) -> DatabaseTarget {
    DatabaseTarget {
        database: reference
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("db").join(format!("{name}.sqlite"))),
        source_root: Some(
            reference
                .source_root
                .clone()
                .or_else(|| {
                    file_config
                        .source_root
                        .clone()
                        .map(|base| join_namespace(base, name))
                })
                .unwrap_or_else(|| PathBuf::from("src").join(name)),
        ),
        source_root_namespace: Some(name.to_string()),
        output: reference.output.clone(),
        init_sql: reference
            .init_sql
            .clone()
            .or_else(|| file_config.init_sql.clone()),
        migrations_dir: Some(
            reference
                .migrations_dir
                .clone()
                .or_else(|| {
                    file_config
                        .migrations_dir
                        .clone()
                        .map(|base| join_namespace(base, name))
                })
                .unwrap_or_else(|| PathBuf::from("db/migrations").join(name)),
        ),
        bootstrap_dir: reference.bootstrap_dir.clone().or_else(|| {
            file_config
                .bootstrap_dir
                .clone()
                .map(|base| join_namespace(base, name))
        }),
        seeds_dir: Some(
            reference
                .seeds_dir
                .clone()
                .or_else(|| {
                    file_config
                        .seeds_dir
                        .clone()
                        .map(|base| join_namespace(base, name))
                })
                .unwrap_or_else(|| PathBuf::from("db/seeds").join(name)),
        ),
        migration_table: reference
            .migration_table
            .clone()
            .or_else(|| file_config.migration_table.clone()),
        schema_output: reference.schema_output.clone(),
    }
}

fn join_namespace(base: PathBuf, name: &str) -> PathBuf {
    if base.file_name().and_then(|value| value.to_str()) == Some(name) {
        base
    } else {
        base.join(name)
    }
}

fn validate_output_under_source_root(source_root: &Path, output: &Path) -> Result<(), ConfigError> {
    let source_root = normalize_for_comparison(source_root);
    let output = normalize_for_comparison(output);
    if output.starts_with(&source_root) {
        Ok(())
    } else {
        Err(ConfigError::OutputOutsideSourceRoot {
            output,
            source_root,
        })
    }
}

fn normalize_for_comparison(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    normalize_path(&absolute)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
