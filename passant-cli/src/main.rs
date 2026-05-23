use anyhow::Context;
use clap::{Parser, Subcommand};
use passant_core::{
    PassantPlanner, PassantRewriter, PolicyIr, Resolution, parse_policy_text, parse_query_to_ir,
};

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
        #[arg(long)]
        policy: Vec<String>,
    },
    Explain {
        sql: String,
        #[arg(long)]
        policy: Vec<String>,
    },
    Plan {
        sql: String,
        #[arg(long)]
        policy: Vec<String>,
    },
    ParsePolicy {
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        source: Vec<String>,
        #[arg(long)]
        sink: Option<String>,
        #[arg(long)]
        dimension: Vec<String>,
        #[arg(long)]
        constraint: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Rewrite { sql, policy } => {
            let mut rewriter = PassantRewriter::new();
            for policy_text in policy {
                rewriter
                    .register_policy_text(&policy_text)
                    .context("failed to parse policy")?;
            }
            println!(
                "{}",
                rewriter.rewrite(&sql).context("failed to rewrite SQL")?
            );
        }
        Commands::Explain { sql, policy } => {
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let policies = parse_policy_args(policy)?;
            let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
            println!("{}", serde_json::to_string_pretty(&explanation)?);
        }
        Commands::Plan { sql, policy } => {
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let policies = parse_policy_args(policy)?;
            let result = PassantPlanner::new().plan_query(&ir, &policies);
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Commands::ParsePolicy {
            text,
            source,
            sink,
            dimension,
            constraint,
        } => {
            let policy = if let Some(text) = text {
                parse_policy_text(&text).context("failed to parse policy")?
            } else {
                let constraint = constraint.context("--constraint is required without --text")?;
                PolicyIr::CompatDfc {
                    sources: source,
                    required_sources: Vec::new(),
                    dimensions: dimension,
                    sink,
                    sink_alias: None,
                    constraint,
                    on_fail: Resolution::Remove,
                    description: None,
                }
            };
            println!("{}", serde_json::to_string_pretty(&policy)?);
        }
    }
    Ok(())
}

fn parse_policy_args(policy_texts: Vec<String>) -> anyhow::Result<Vec<PolicyIr>> {
    policy_texts
        .into_iter()
        .map(|policy_text| parse_policy_text(&policy_text).context("failed to parse policy"))
        .collect()
}
