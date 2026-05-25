use anyhow::Context;
use clap::{Parser, Subcommand};
use passant_core::{
    PassantPlanner, PassantRewriter, PolicyIr, Resolution, RewriteOptions, parse_policy_text,
    parse_query_to_ir,
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
        #[arg(long, help = "Print rewrite stats JSON after rewritten SQL")]
        stats: bool,
    },
    Explain {
        sql: String,
        #[arg(long)]
        policy: Vec<String>,
        #[arg(
            long,
            help = "Run a rewrite with stats collection and include timings in output"
        )]
        stats: bool,
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
        Commands::Rewrite { sql, policy, stats } => {
            let mut rewriter = PassantRewriter::new();
            for policy_text in policy {
                rewriter
                    .register_policy_text(&policy_text)
                    .context("failed to parse policy")?;
            }
            let options = RewriteOptions {
                collect_stats: stats,
                ..RewriteOptions::default()
            };
            let rewritten = rewriter
                .rewrite_with_options(&sql, options)
                .context("failed to rewrite SQL")?;
            println!("{rewritten}");
            if stats {
                let export = passant_core::RewriteStatsExport::from(rewriter.last_rewrite_stats());
                eprintln!("{}", serde_json::to_string_pretty(&export)?);
            }
        }
        Commands::Explain { sql, policy, stats } => {
            let mut rewriter = PassantRewriter::new();
            for policy_text in policy {
                rewriter
                    .register_policy_text(&policy_text)
                    .context("failed to parse policy")?;
            }
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let mut explanation = PassantPlanner::new().explain_rewrite(&ir, &rewriter.policies());
            if stats {
                rewriter
                    .rewrite_with_options(
                        &sql,
                        RewriteOptions {
                            collect_stats: true,
                            ..RewriteOptions::default()
                        },
                    )
                    .context("failed to rewrite SQL for stats")?;
                explanation.rewrite_stats = Some(rewriter.last_rewrite_stats().into());
            }
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
