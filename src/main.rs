use std::path::PathBuf;

use clap::{Parser, Subcommand};
use marmot::{Config, Target, analyze_project, emit_project, migrations, reset, seeds};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Generate typed database code from colocated SQL files"
)]
struct Cli {
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
    database: PathBuf,

    #[arg(long, default_value = "src")]
    source_root: PathBuf,

    #[arg(long)]
    sql_dir: Option<PathBuf>,

    #[arg(long, default_value = "src/generated/sql")]
    output: PathBuf,

    #[arg(long, value_enum, default_value_t = Target::Rust)]
    target: Target,

    #[arg(long)]
    check: bool,
}

#[derive(Debug, Parser)]
struct MigrateArgs {
    #[arg(long)]
    database: PathBuf,

    #[arg(long, default_value = migrations::MIGRATION_DIR)]
    migrations_dir: PathBuf,
}

#[derive(Debug, Parser)]
struct SeedArgs {
    #[arg(long)]
    database: PathBuf,

    #[arg(long, default_value = seeds::SEED_DIR)]
    seeds_dir: PathBuf,
}

#[derive(Debug, Parser)]
struct ResetArgs {
    #[arg(long)]
    database: PathBuf,

    #[arg(long, default_value = migrations::MIGRATION_DIR)]
    migrations_dir: PathBuf,

    #[arg(long, default_value = seeds::SEED_DIR)]
    seeds_dir: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect(args) => {
            let project = analyze_project(&config(args))?;
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
            let config = config(args);
            let project = analyze_project(&config)?;
            emit_project(&config, &project)?;
        }
        Command::Migrate(args) => {
            let applied = migrations::migrate_from(args.database, args.migrations_dir)?;
            print_applied("Applied", &applied);
        }
        Command::Seed(args) => {
            let applied = seeds::seed_from(args.database, args.seeds_dir)?;
            print_applied("Ran", &applied);
        }
        Command::Reset(args) => {
            let (applied_migrations, applied_seeds) =
                reset::reset_from(args.database, args.migrations_dir, args.seeds_dir)?;
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

fn config(args: Args) -> Config {
    Config {
        database: args.database,
        source_root: args.source_root,
        sql_dir: args.sql_dir,
        output: args.output,
        target: args.target,
        check: args.check,
    }
}
