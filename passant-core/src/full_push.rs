//! Full-Push rewrite: inline policy enforcement (WHERE/HAVING/join pushdown) on the
//! original query shape.

use std::time::Instant;

use crate::optimizer::RewriteStrategy;
use crate::rewrite_strategy::{RewriteAttempt, RewriteEngine, RewriteRequest, StatementKind};
use crate::rewriter::{PassantRewriter, RewriteError};

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
        request.semiring.all_distributive && !request.options.use_partial_push
    }

    fn rewrite(
        &self,
        rewriter: &PassantRewriter,
        request: &RewriteRequest<'_>,
    ) -> Result<RewriteAttempt, RewriteError> {
        let mut statement = request.statement.clone();
        rewriter.rewrite_statement_full_push(&mut statement, request.options.collect_stats)?;
        let format_start = Instant::now();
        let rewritten = crate::sql::render_statement(&statement, None);
        if request.options.collect_stats {
            rewriter.stats.add_elapsed_format(format_start.elapsed());
        }
        Ok(RewriteAttempt::Applied(rewritten))
    }
}
