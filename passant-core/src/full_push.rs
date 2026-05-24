//! Full-Push rewrite: inline policy enforcement (WHERE/HAVING/join pushdown) on the
//! original query shape.

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
        if rewriter.policies().is_empty() {
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
        rewriter.rewrite_statement_full_push(&mut statement)?;
        Ok(RewriteAttempt::Applied(statement.to_string()))
    }
}
