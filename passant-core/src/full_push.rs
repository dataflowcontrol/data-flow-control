//! Full-Push rewrite: inline policy enforcement (WHERE/HAVING/join pushdown) on the
//! original query shape.

use std::time::Instant;

use crate::optimizer::RewriteStrategy;
use crate::rewrite_strategy::{RewriteAttempt, RewriteEngine, RewriteRequest, StatementKind};
use crate::rewriter::{PassantRewriter, RewriteError};
use crate::semiring::SemiringAnalysis;

fn semiring_allows_full_push(rewriter: &PassantRewriter, semiring: &SemiringAnalysis) -> bool {
    if semiring.all_distributive {
        return true;
    }
    semiring
        .non_distributive_aggregates
        .iter()
        .all(|aggregate| count_if_style_scan_aggregate(rewriter, aggregate))
}

fn count_if_style_scan_aggregate(rewriter: &PassantRewriter, aggregate: &str) -> bool {
    let lower = aggregate.to_ascii_lowercase();
    if lower.contains("array_agg")
        || lower == "list"
        || lower.contains("string_agg")
        || lower.contains("median")
    {
        return false;
    }
    rewriter.aggregate_registry.is_scan_transformable(aggregate)
}

/// Full-push engine — mutates the query in place, preserving its overall shape.
pub struct FullPushEngine;

impl RewriteEngine for FullPushEngine {
    fn kind(&self) -> RewriteStrategy {
        RewriteStrategy::FullPush
    }

    fn priority(&self) -> u8 {
        0
    }

    fn matches(&self, rewriter: &PassantRewriter, request: &RewriteRequest<'_>) -> bool {
        if !rewriter.has_registered_policies() {
            return false;
        }
        if matches!(request.kind, StatementKind::Passthrough) {
            return false;
        }
        !request.options.use_partial_push && semiring_allows_full_push(rewriter, &request.semiring)
    }

    fn rewrite(
        &self,
        rewriter: &PassantRewriter,
        request: &RewriteRequest<'_>,
    ) -> Result<RewriteAttempt, RewriteError> {
        let mut statement = request.statement.clone();
        rewriter.rewrite_statement_full_push(&mut statement, &request.options)?;
        let format_start = Instant::now();
        let rewritten = crate::sql::render_statement(&statement, None);
        if request.options.collect_stats {
            rewriter.stats.add_elapsed_format(format_start.elapsed());
        }
        Ok(RewriteAttempt::Applied(rewritten))
    }
}
