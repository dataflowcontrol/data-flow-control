use anyhow::Context;
use clap::{Parser, Subcommand};
use passant_core::{PassantPlanner, PolicyIr, Resolution, parse_query_to_ir};

#[derive(Debug, Parser)]
#[command(name = "passant")]
#[command(about = "Passant query planner and rewrite explainer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Rewrite {
        sql: String,
    },
    Explain {
        sql: String,
    },
    Plan {
        sql: String,
    },
    ParsePolicy {
        #[arg(long)]
        source: Vec<String>,
        #[arg(long)]
        sink: Option<String>,
        #[arg(long)]
        constraint: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Rewrite { sql } => {
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let result = PassantPlanner::new().plan_query(&ir, &[]);
            println!("{}", result.chosen.rewritten_sql);
        }
        Commands::Explain { sql } => {
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let explanation = PassantPlanner::new().explain_rewrite(&ir, &[]);
            println!("{}", serde_json::to_string_pretty(&explanation)?);
        }
        Commands::Plan { sql } => {
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let result = PassantPlanner::new().plan_query(&ir, &[]);
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::ParsePolicy {
            source,
            sink,
            constraint,
        } => {
            let policy = PolicyIr::CompatDfc {
                sources: source,
                sink,
                sink_alias: None,
                constraint,
                on_fail: Resolution::Remove,
                description: None,
            };
            println!("{}", serde_json::to_string_pretty(&policy)?);
        }
    }
    Ok(())
}
