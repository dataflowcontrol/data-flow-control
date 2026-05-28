use std::collections::HashSet;
use std::sync::Mutex;

use sqlparser::ast::Statement;

use crate::identifiers::TableKey;
use crate::policy_store::{MultiSourceLookupMode, PolicyStore};
use crate::query_analysis::StatementAnalysis;
use crate::rewriter::policy_expr::compiled_policy_applicability;
use crate::rewriter::types::PolicyApplicability;

use super::dominance::resolve_scope_policies_with_dominance;

/// Aggregated planning diagnostics across all SELECT scopes in one statement rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatementRewriteSummary {
    pub scope_diagnostics: Vec<ScopePlanDiagnostics>,
}

impl StatementRewriteSummary {
    pub fn push_scope(&mut self, diagnostics: ScopePlanDiagnostics) {
        self.scope_diagnostics.push(diagnostics);
    }

    pub fn aggregate(&self) -> ScopePlanDiagnostics {
        self.scope_diagnostics
            .iter()
            .fold(ScopePlanDiagnostics::default(), |mut acc, scope| {
                acc.candidate_policies += scope.candidate_policies;
                acc.applicable_policies += scope.applicable_policies;
                acc.dominated_policies += scope.dominated_policies;
                acc.skipped_pushdown += scope.skipped_pushdown;
                acc.skipped_exists_handled += scope.skipped_exists_handled;
                acc.emitted_policy_actions += scope.emitted_policy_actions;
                acc
            })
    }
}

#[derive(Debug, Default)]
pub(crate) struct StatementRewriteSummaryCell {
    inner: Mutex<StatementRewriteSummary>,
}

impl StatementRewriteSummaryCell {
    pub fn reset(&self) {
        if let Ok(mut summary) = self.inner.lock() {
            summary.scope_diagnostics.clear();
        }
    }

    pub fn record_scope(&self, diagnostics: ScopePlanDiagnostics) {
        if let Ok(mut summary) = self.inner.lock() {
            summary.push_scope(diagnostics);
        }
    }

    pub fn snapshot(&self) -> StatementRewriteSummary {
        self.inner
            .lock()
            .map(|summary| summary.clone())
            .unwrap_or_default()
    }
}

/// Counts produced while planning policy actions for one SELECT scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScopePlanDiagnostics {
    pub candidate_policies: usize,
    pub applicable_policies: usize,
    pub dominated_policies: usize,
    pub skipped_pushdown: usize,
    pub skipped_exists_handled: usize,
    pub emitted_policy_actions: usize,
}

/// Resolve indexed candidates, applicability, and dominance for a scope.
pub(crate) fn resolve_scope_policies<'a>(
    store: &'a PolicyStore,
    direct_tables: &HashSet<TableKey>,
    sink: Option<&str>,
    allow_partial_source_visibility: bool,
    exclude_pushdown: &HashSet<usize>,
    exclude_exists: &HashSet<usize>,
) -> (
    Vec<(usize, &'a crate::policy::PolicyIr, PolicyApplicability)>,
    ScopePlanDiagnostics,
) {
    let sink_key = sink.map(TableKey::new);
    let multi_source_mode = if allow_partial_source_visibility {
        MultiSourceLookupMode::AnyOverlap
    } else {
        MultiSourceLookupMode::Subset
    };
    store.debug_assert_candidates_match_slow_scan(
        direct_tables,
        sink_key.as_ref(),
        multi_source_mode,
    );
    let mut candidate_count = 0usize;
    let mut skipped_pushdown = 0usize;
    let mut skipped_exists = 0usize;
    let indexed_applicable = store
        .candidate_scope_lookup(direct_tables, sink_key.as_ref(), multi_source_mode)
        .iter()
        .filter(|index| {
            candidate_count += 1;
            if exclude_pushdown.contains(index) {
                skipped_pushdown += 1;
                return false;
            }
            if exclude_exists.contains(index) {
                skipped_exists += 1;
                return false;
            }
            true
        })
        .filter_map(|index| {
            let compiled = store.compiled(index)?;
            compiled_policy_applicability(
                compiled,
                direct_tables,
                sink_key.as_ref(),
                allow_partial_source_visibility,
            )
            .map(|applicability| (index, &compiled.policy, applicability))
        })
        .collect::<Vec<_>>();
    resolve_scope_policies_with_dominance(
        store,
        indexed_applicable,
        candidate_count,
        skipped_pushdown,
        skipped_exists,
    )
}

/// Plan candidate/applicable counts for every SELECT scope in a statement without mutating SQL.
pub fn plan_statement_rewrite_summary(
    store: &PolicyStore,
    statement: &Statement,
) -> StatementRewriteSummary {
    let analysis = StatementAnalysis::from_statement(statement);
    let sink = analysis.sink.as_ref().map(|key| key.as_str());
    let mut summary = StatementRewriteSummary::default();
    for select_analysis in analysis.select_scopes {
        let (_, diagnostics) = resolve_scope_policies(
            store,
            &select_analysis.scope.direct_base_tables,
            sink,
            false,
            &HashSet::new(),
            &HashSet::new(),
        );
        summary.push_scope(diagnostics);
    }
    summary
}

/// True when indexed lookup finds an enforcement policy for the scope (Partial-Push gate).
pub(crate) fn scope_has_enforcement_policies(
    store: &PolicyStore,
    direct_tables: &HashSet<TableKey>,
    exists_subquery_tables: &HashSet<TableKey>,
) -> bool {
    let mut candidate_tables = direct_tables.clone();
    candidate_tables.extend(exists_subquery_tables.iter().cloned());
    let candidate_lookup = store.enforcement_candidate_lookup(&candidate_tables);
    candidate_lookup.iter().any(|index| {
        let Some(compiled) = store.compiled(index) else {
            return false;
        };
        if compiled_policy_applicability(compiled, direct_tables, None, false).is_some() {
            return true;
        }
        compiled.source_keys.iter().any(|source| {
            exists_subquery_tables.contains(source) && !direct_tables.contains(source)
        })
    })
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    #[test]
    fn statement_rewrite_summary_aggregates_scope_diagnostics() {
        let mut summary = StatementRewriteSummary::default();
        summary.push_scope(ScopePlanDiagnostics {
            candidate_policies: 2,
            applicable_policies: 1,
            dominated_policies: 1,
            ..ScopePlanDiagnostics::default()
        });
        summary.push_scope(ScopePlanDiagnostics {
            candidate_policies: 3,
            applicable_policies: 2,
            dominated_policies: 0,
            emitted_policy_actions: 2,
            ..ScopePlanDiagnostics::default()
        });
        let total = summary.aggregate();
        assert_eq!(total.candidate_policies, 5);
        assert_eq!(total.applicable_policies, 3);
        assert_eq!(total.dominated_policies, 1);
        assert_eq!(total.emitted_policy_actions, 2);
    }
}
