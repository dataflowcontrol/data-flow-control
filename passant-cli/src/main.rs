use anyhow::Context;
use clap::{Parser, Subcommand};
use passant_core::{
    PassantPlanner, PassantRewriter, PolicyIr, Resolution, RewriteOptions, parse_policy_text,
    parse_query_to_ir,
};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "passant")]
#[command(
    about = "Passant query planner and rewrite explainer (paper PGN, REMOVE/KILL/UDF resolutions)"
)]
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
        #[arg(
            long,
            help = "Path to explicit CatalogSnapshot JSON (required for policy SOURCE/SINK registration)"
        )]
        catalog: Option<PathBuf>,
        #[arg(long, help = "Print rewrite stats JSON after rewritten SQL")]
        stats: bool,
    },
    Explain {
        sql: String,
        #[arg(long)]
        policy: Vec<String>,
        #[arg(
            long,
            help = "Path to explicit CatalogSnapshot JSON (required for policy SOURCE/SINK registration)"
        )]
        catalog: Option<PathBuf>,
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
        #[arg(
            long,
            help = "Path to explicit CatalogSnapshot JSON (required for policy SOURCE/SINK registration)"
        )]
        catalog: Option<PathBuf>,
    },
    ParsePolicy {
        #[arg(
            long,
            help = "PGN policy text (SOURCE/SINK/DIMENSION/UNIQUE/CONSTRAINT/ON FAIL)"
        )]
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
        Commands::Rewrite {
            sql,
            policy,
            catalog,
            stats,
        } => {
            let rewriter = configured_rewriter(catalog, policy)?;
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
        Commands::Explain {
            sql,
            policy,
            catalog,
            stats,
        } => {
            let rewriter = configured_rewriter(catalog, policy)?;
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let explanation =
                PassantPlanner::new().explain_rewrite_with_rewriter(&ir, &rewriter, stats);
            println!("{}", serde_json::to_string_pretty(&explanation)?);
        }
        Commands::Plan {
            sql,
            policy,
            catalog,
        } => {
            let ir = parse_query_to_ir(&sql).context("failed to parse SQL")?;
            let rewriter = configured_rewriter(catalog, policy)?;
            let result = PassantPlanner::new().plan_query_with_rewriter(&ir, &rewriter);
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
                PolicyIr::Pgn {
                    sources: source,
                    required_sources: Vec::new(),
                    dimension_tables: dimension,
                    dimension_aliases: std::collections::HashMap::new(),
                    dimension_queries: std::collections::HashMap::new(),
                    sink,
                    sink_alias: None,
                    source_aliases: std::collections::HashMap::new(),
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

fn configured_rewriter(
    catalog: Option<PathBuf>,
    policies: Vec<String>,
) -> anyhow::Result<PassantRewriter> {
    let mut rewriter = PassantRewriter::new();
    if let Some(path) = catalog {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read catalog {}", path.display()))?;
        let snapshot: passant_core::catalog::CatalogSnapshot =
            serde_json::from_slice(&bytes).context("invalid CatalogSnapshot JSON")?;
        rewriter.apply_catalog_snapshot(snapshot);
    }
    for policy in policies {
        rewriter
            .register_policy_text(&policy)
            .context("failed to register policy against explicit catalog")?;
    }
    Ok(rewriter)
}
