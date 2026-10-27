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
        rewriter
            .catalog_mut()
            .register_table("foo", vec!["id".into()]);
        rewriter
            .register_policy(remove_policy("foo", "foo.id > 1"))
            .expect("fixture catalog");
        rewriter
            .catalog_mut()
            .register_table("bar", vec!["id".into()]);
        rewriter
            .register_policy(remove_policy("bar", "bar.id > 1"))
            .expect("fixture catalog");
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
        rewriter
            .catalog_mut()
            .register_table("foo", vec!["id".into()]);
        rewriter
            .register_policy(remove_policy("foo", "foo.id > 1"))
            .expect("fixture catalog");
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
        rewriter
            .catalog_mut()
            .register_table("foo", vec!["id".into()]);
        rewriter
            .register_policy(remove_policy("foo", "foo.id > 1"))
            .expect("fixture catalog");
        rewriter
            .catalog_mut()
            .register_table("bar", vec!["id".into()]);
        rewriter
            .register_policy(remove_policy("bar", "bar.id > 1"))
            .expect("fixture catalog");
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

    #[test]
    fn kill_policy_with_introspected_arbitrary_emits_tuple_action() {
        use crate::aggregate_registry::AggregateFunctionSnapshot;
        use crate::catalog::{CatalogSnapshot, CatalogTableInfo};
        use std::collections::HashMap;

        let mut tables = HashMap::new();
        tables.insert(
            "Receipts".to_string(),
            CatalogTableInfo {
                columns: vec!["cat".into()],
                ..CatalogTableInfo::default()
            },
        );
        tables.insert(
            "Expenses".to_string(),
            CatalogTableInfo {
                columns: vec!["biz_use".into()],
                ..CatalogTableInfo::default()
            },
        );
        let mut rewriter = PassantRewriter::new();
        rewriter.apply_catalog_snapshot(CatalogSnapshot {
            dialect: Some("duckdb".to_string()),
            tables,
            aggregate_functions: vec![AggregateFunctionSnapshot {
                name: "arbitrary".to_string(),
                schema: None,
                aliases: vec![],
                classification: None,
                source: Some("introspected".to_string()),
            }],
            ..CatalogSnapshot::default()
        });
        rewriter
            .register_validated_policy(
                crate::parse_policy_text(
                    "SOURCE Receipts SINK Expenses CONSTRAINT ARBITRARY(Receipts.cat) != 'Meal' OR Expenses.biz_use <= 50.0 ON FAIL KILL",
                )
                .expect("parse"),
            )
            .expect("register");
        let statement =
            crate::parser::parse_query("INSERT INTO Expenses SELECT 1, 100.0 FROM Receipts")
                .expect("parse");
        let sqlparser::ast::Statement::Insert(insert) = statement else {
            panic!("expected insert");
        };
        let Some(mut query) = insert.source else {
            panic!("expected insert source");
        };
        let sqlparser::ast::SetExpr::Select(select) = query.body.as_mut() else {
            panic!("expected select");
        };
        let analysis = SelectAnalysis::from_select(select);
        let context = RewriteContext {
            sink: Some("Expenses".to_string()),
            sink_expr_by_column: HashMap::from([(
                "biz_use".to_string(),
                sqlparser::ast::Expr::Value(sqlparser::ast::Value::Number("100.0".into(), false)),
            )]),
            ..RewriteContext::default()
        };
        let plan = plan_select_rewrite(
            rewriter.policy_store(),
            rewriter.catalog(),
            None,
            select.as_mut(),
            &analysis,
            &context,
            &HashSet::new(),
        )
        .expect("plan should succeed");
        assert_eq!(
            plan.policy_actions.len(),
            1,
            "expected one kill action, diagnostics={:?}",
            plan.diagnostics
        );
    }
}
