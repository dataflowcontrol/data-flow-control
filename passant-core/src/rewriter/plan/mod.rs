//! Rewrite planning: candidate lookup, dominance, and action emission without AST mutation.

mod actions;
mod applicability;
mod dominance;
mod scope_plan;
mod write_plan;

pub(crate) use actions::{
    PolicyResolutionAction, apply_policy_resolution_actions, plan_policy_filter_actions,
    relation_udf_names, relation_violation_filters,
};
pub use applicability::{
    ScopePlanDiagnostics, StatementRewriteSummary, plan_statement_rewrite_summary,
};
pub(crate) use applicability::{
    StatementRewriteSummaryCell, resolve_scope_policies, scope_has_enforcement_policies,
};
pub use scope_plan::SelectRewritePlan;
pub(crate) use scope_plan::{apply_select_rewrite_plan, plan_select_rewrite};
pub(crate) use write_plan::{
    apply_update_scope_plan, plan_merge_source_filters, plan_update_scope,
};

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::identifiers::TableKey;
    use crate::policy::{PolicyIr, Resolution};
    use crate::query_analysis::SelectAnalysis;
    use crate::rewriter::{PassantRewriter, RewriteContext};

    fn remove_policy(source: &str, constraint: &str) -> PolicyIr {
        PolicyIr::Pgn {
            sources: vec![source.to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: constraint.to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }
    }

    #[test]
    fn resolve_scope_policies_matches_indexed_candidates() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(remove_policy("foo", "foo.id > 1"));
        rewriter.register_policy(remove_policy("bar", "bar.id > 1"));
        let tables = HashSet::from([TableKey::new("foo")]);
        let (applicable, diag) = resolve_scope_policies(
            rewriter.policy_store(),
            &tables,
            None,
            false,
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(diag.candidate_policies, 1);
        assert_eq!(applicable.len(), 1);
        assert_eq!(applicable[0].1.sources(), &["foo"]);
    }

    #[test]
    fn plan_select_rewrite_emits_where_action_for_scan_policy() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(remove_policy("foo", "foo.id > 1"));
        let statement = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::GenericDialect {},
            "SELECT id FROM foo",
        )
        .expect("parse")
        .pop()
        .expect("statement");
        let sqlparser::ast::Statement::Query(mut query) = statement else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(select) = query.body.as_mut() else {
            panic!("expected select");
        };
        let analysis = SelectAnalysis::from_select(select);
        let context = RewriteContext::default();
        let plan = plan_select_rewrite(
            rewriter.policy_store(),
            rewriter.catalog(),
            None,
            select.as_mut(),
            &analysis,
            &context,
            &HashSet::new(),
        )
        .expect("plan");
        assert_eq!(plan.diagnostics.candidate_policies, 1);
        assert_eq!(plan.diagnostics.emitted_policy_actions, 1);
        assert!(matches!(
            plan.policy_actions.first(),
            Some(PolicyResolutionAction::Filter { .. })
        ));
    }

    #[test]
    fn plan_statement_rewrite_summary_counts_each_select_scope() {
        let mut rewriter = PassantRewriter::new();
        rewriter.register_policy(remove_policy("foo", "foo.id > 1"));
        rewriter.register_policy(remove_policy("bar", "bar.id > 1"));
        let statement = sqlparser::parser::Parser::parse_sql(
            &sqlparser::dialect::GenericDialect {},
            "WITH cte AS (SELECT id FROM bar) SELECT id FROM foo JOIN cte ON foo.id = cte.id",
        )
        .expect("parse")
        .pop()
        .expect("statement");
        let summary = plan_statement_rewrite_summary(rewriter.policy_store(), &statement);
        assert_eq!(summary.scope_diagnostics.len(), 2);
        assert_eq!(summary.aggregate().candidate_policies, 2);
    }
}
