use std::path::PathBuf;

use clap::{Parser, Subcommand};
use marmot::{Config, Target, analyze_project, emit_project};

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
    }
    Ok(())
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
