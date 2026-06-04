use crate::policy::PolicyIr;
use crate::policy_store::PolicyStore;
use crate::rewriter::helpers::prune_dominated_applicable_with_store;
use crate::rewriter::types::PolicyApplicability;

use super::ScopePlanDiagnostics;

/// Resolve indexed candidates, prune dominated policies, and return applicability pairs.
pub(crate) fn resolve_scope_policies_with_dominance<'a>(
    store: &'a PolicyStore,
    indexed_applicable: Vec<(usize, &'a PolicyIr, PolicyApplicability)>,
    candidate_count: usize,
    skipped_pushdown: usize,
    skipped_exists: usize,
) -> (
    Vec<(usize, &'a PolicyIr, PolicyApplicability)>,
    ScopePlanDiagnostics,
) {
    let mut diagnostics = ScopePlanDiagnostics {
        candidate_policies: candidate_count,
        skipped_pushdown,
        skipped_exists_handled: skipped_exists,
        applicable_policies: indexed_applicable.len(),
        ..ScopePlanDiagnostics::default()
    };
    if indexed_applicable.is_empty() {
        return (Vec::new(), diagnostics);
    }
    let (applicable, dominated) = prune_dominated_applicable_with_store(store, indexed_applicable);
    diagnostics.dominated_policies = dominated;
    (applicable, diagnostics)
}
