use std::path::PathBuf;

use clap::{Parser, Subcommand};
use marmot::{
    Config, FileConfig, Target, analyze_project, config::ConfigError, emit_project, migrations,
    reset, seeds,
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
    migrations_dir: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct SeedArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    seeds_dir: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct ResetArgs {
    #[arg(long)]
    database: Option<PathBuf>,

    #[arg(long)]
    migrations_dir: Option<PathBuf>,

    #[arg(long)]
    seeds_dir: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let file_config = FileConfig::read_optional(&cli.config)?;
    match cli.command {
        Command::Inspect(args) => {
            let project = analyze_project(&config(args, &file_config)?)?;
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
        Command::Generate(args) => {
            let config = config(args, &file_config)?;
            let project = analyze_project(&config)?;
            emit_project(&config, &project)?;
        }
        Command::Migrate(args) => {
            let database = database_path(args.database, &file_config)?;
            let migrations_dir = args
                .migrations_dir
                .or_else(|| file_config.migrations_dir.clone())
                .unwrap_or_else(|| PathBuf::from(migrations::MIGRATION_DIR));
            let applied = migrations::migrate_from(database, migrations_dir)?;
            print_applied("Applied", &applied);
        }
        Command::Seed(args) => {
            let database = database_path(args.database, &file_config)?;
            let seeds_dir = args
                .seeds_dir
                .or_else(|| file_config.seeds_dir.clone())
                .unwrap_or_else(|| PathBuf::from(seeds::SEED_DIR));
            let applied = seeds::seed_from(database, seeds_dir)?;
            print_applied("Ran", &applied);
        }
        Command::Reset(args) => {
            let database = database_path(args.database, &file_config)?;
            let migrations_dir = args
                .migrations_dir
                .or_else(|| file_config.migrations_dir.clone())
                .unwrap_or_else(|| PathBuf::from(migrations::MIGRATION_DIR));
            let seeds_dir = args
                .seeds_dir
                .or_else(|| file_config.seeds_dir.clone())
                .unwrap_or_else(|| PathBuf::from(seeds::SEED_DIR));
            let (applied_migrations, applied_seeds) =
                reset::reset_from(database, migrations_dir, seeds_dir)?;
            print_applied("Applied", &applied_migrations);
            print_applied("Ran", &applied_seeds);
        }
    }
    Ok(())
}

fn print_applied(action: &str, applied: &[String]) {
    for version in applied {
        println!("{action} {version}");
    }
}

fn config(args: Args, file_config: &FileConfig) -> Result<Config, ConfigError> {
    Ok(Config {
        database: database_path(args.database, file_config)?,
        source_root: args
            .source_root
            .or_else(|| file_config.source_root.clone())
            .unwrap_or_else(|| PathBuf::from("src")),
        sql_dir: args.sql_dir.or_else(|| file_config.sql_dir.clone()),
        output: args
            .output
            .or_else(|| file_config.output.clone())
            .unwrap_or_else(|| PathBuf::from("src/generated/sql")),
        target: args.target,
        check: args.check,
    })
}

fn database_path(
    cli_database: Option<PathBuf>,
    file_config: &FileConfig,
) -> Result<PathBuf, ConfigError> {
    cli_database
        .or_else(|| std::env::var_os("DATABASE_URL").map(PathBuf::from))
        .or_else(|| file_config.database.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or(ConfigError::MissingDatabase)
}
