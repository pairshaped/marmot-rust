use std::path::PathBuf;

use clap::{Parser, Subcommand};
use marmot::{
    Config, FileConfig, Target, analyze_project,
    config::{ConfigError, DatabaseReference},
    emit_project, migrations, reset, seeds,
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
    Seed(SeedArgs),
    Reset(ResetArgs),
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
    sql_dir: Option<PathBuf>,

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
struct ResetArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    database_name: Option<String>,

    #[arg(long)]
    migrations_dir: Option<PathBuf>,

    #[arg(long)]
    seeds_dir: Option<PathBuf>,
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
            for config in configs(args, &file_config)? {
                let project = analyze_project(&config)?;
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
            for config in configs(args, &file_config)? {
                let project = analyze_project(&config)?;
                emit_project(&config, &project)?;
            }
        }
        Command::Migrate(args) => {
            for target in database_targets(args.database, args.database_name, &file_config)? {
                let migrations_dir = args
                    .migrations_dir
                    .clone()
                    .or(target.migrations_dir)
                    .unwrap_or_else(|| PathBuf::from(migrations::MIGRATION_DIR));
                let applied = migrations::migrate_from(target.database, migrations_dir)?;
                print_applied("Applied", &applied);
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
        Command::Reset(args) => {
            for target in database_targets(args.database, args.database_name, &file_config)? {
                let migrations_dir = args
                    .migrations_dir
                    .clone()
                    .or(target.migrations_dir)
                    .unwrap_or_else(|| PathBuf::from(migrations::MIGRATION_DIR));
                let seeds_dir = args
                    .seeds_dir
                    .clone()
                    .or(target.seeds_dir)
                    .unwrap_or_else(|| PathBuf::from(seeds::SEED_DIR));
                let (applied_migrations, applied_seeds) =
                    reset::reset_from(target.database, migrations_dir, seeds_dir)?;
                print_applied("Applied", &applied_migrations);
                print_applied("Ran", &applied_seeds);
            }
        }
    }
    Ok(())
}

fn print_applied(action: &str, applied: &[String]) {
    for version in applied {
        println!("{action} {version}");
    }
}

fn configs(args: Args, file_config: &FileConfig) -> Result<Vec<Config>, ConfigError> {
    let targets = database_targets(args.database, args.database_name, file_config)?;
    let source_root = args
        .source_root
        .or_else(|| file_config.source_root.clone())
        .unwrap_or_else(|| PathBuf::from("src"));
    let sql_dir = args.sql_dir;
    let output = args.output;
    let target = args.target;
    let check = args.check;

    Ok(targets
        .into_iter()
        .map(|database_target| Config {
            database: database_target.database,
            source_root: source_root.clone(),
            sql_dir: sql_dir.clone().or(database_target.sql_dir),
            output: output
                .clone()
                .or(database_target.output)
                .unwrap_or_else(|| PathBuf::from("src/generated/sql")),
            target,
            check,
        })
        .collect())
}

#[derive(Debug)]
struct DatabaseTarget {
    database: PathBuf,
    sql_dir: Option<PathBuf>,
    output: Option<PathBuf>,
    migrations_dir: Option<PathBuf>,
    seeds_dir: Option<PathBuf>,
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

    if let Some(database) = explicit_database_path(cli_database, file_config) {
        return Ok(vec![DatabaseTarget {
            database,
            sql_dir: file_config.sql_dir.clone(),
            output: file_config.output.clone(),
            migrations_dir: file_config.migrations_dir.clone(),
            seeds_dir: file_config.seeds_dir.clone(),
        }]);
    }

    if !file_config.databases.is_empty() {
        return Ok(file_config
            .databases
            .iter()
            .map(|(name, reference)| named_database_target(name, reference, file_config))
            .collect());
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
            sql_dir: file_config.sql_dir.clone(),
            output: file_config.output.clone(),
            migrations_dir: file_config.migrations_dir.clone(),
            seeds_dir: file_config.seeds_dir.clone(),
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
        sql_dir: Some(
            reference
                .sql_dir
                .clone()
                .or_else(|| {
                    file_config
                        .sql_dir
                        .clone()
                        .map(|base| join_namespace(base, name))
                })
                .unwrap_or_else(|| PathBuf::from("src/sql").join(name)),
        ),
        output: Some(
            reference
                .output
                .clone()
                .or_else(|| {
                    file_config
                        .output
                        .clone()
                        .map(|base| join_namespace(base, name))
                })
                .unwrap_or_else(|| PathBuf::from("src/generated/sql").join(name)),
        ),
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
    }
}

fn join_namespace(base: PathBuf, name: &str) -> PathBuf {
    if base.file_name().and_then(|value| value.to_str()) == Some(name) {
        base
    } else {
        base.join(name)
    }
}
