//! Enforcement decisions for fixed external tool payloads.
//!
//! These are native query-planning decisions, not host-language policy checks.
//! The public tool planner constructs both INSERTs and is the only caller of
//! this module, so the model-shape proof applies to the generated occurrence
//! binding as well as to the modeled input query.
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, FunctionArguments, GroupByExpr, JoinOperator, Query, SetExpr, Statement, TableFactor,
    Visit, Visitor,
};

use crate::diagnostics::RewriteError;
use crate::parser::parse_query_with_dialect;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::BranchPolicyEntry;

use super::resolution::PASSANT_REMOVE_OUTPUT;
use super::{PassantRewriter, RewriteOptions};

pub(crate) struct ToolEnforcementPlan {
    pub publication_sql: String,
    pub sink_kill_preflight_sql: Option<String>,
}

impl PassantRewriter {
    /// Classify the model under the registered policies before correspondence
    /// queries or effects execute. The returned mode is private to the native
    /// planner: a host cannot assert the flat-boundary proof itself.
    pub(crate) fn validate_tool_model_policy_shape(
        &self,
        model: &Query,
    ) -> Result<bool, RewriteError> {
        if !self.has_registered_policies() {
            return Ok(false);
        }
        struct Shape<'a> {
            registry: &'a crate::aggregate_registry::AggregateRegistry,
            queries: usize,
            expression_subquery: bool,
            changes_contributors: bool,
            unsupported_terminal_shape: bool,
        }
        impl Visitor for Shape<'_> {
            type Break = ();
            fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
                self.queries += 1;
                self.unsupported_terminal_shape |= self.queries > 1 || query.with.is_some();
                if let SetExpr::Select(select) = query.body.as_ref() {
                    self.changes_contributors |= select.distinct.is_some()
                        || select.having.is_some()
                        || !matches!(&select.group_by, GroupByExpr::Expressions(items, _) if items.is_empty());
                    self.unsupported_terminal_shape |= select.distinct.is_some();
                    for join in select.from.iter().flat_map(|table| &table.joins) {
                        let nullable = matches!(
                            join.join_operator,
                            JoinOperator::LeftOuter(_)
                                | JoinOperator::RightOuter(_)
                                | JoinOperator::FullOuter(_)
                                | JoinOperator::OuterApply
                        );
                        self.changes_contributors |= nullable;
                        self.unsupported_terminal_shape |= nullable;
                    }
                } else {
                    self.changes_contributors = true;
                    self.unsupported_terminal_shape = true;
                }
                ControlFlow::Continue(())
            }
            fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
                self.unsupported_terminal_shape |= matches!(
                    factor,
                    TableFactor::Derived { .. } | TableFactor::Function { lateral: true, .. }
                );
                ControlFlow::Continue(())
            }
            fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
                self.expression_subquery |= matches!(
                    expr,
                    Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
                );
                if let Expr::Function(function) = expr {
                    self.expression_subquery |=
                        matches!(function.args, FunctionArguments::Subquery(_));
                    self.changes_contributors |=
                        function.over.is_some() || self.registry.is_aggregate_call(function);
                }
                ControlFlow::Continue(())
            }
        }
        let mut shape = Shape {
            registry: &self.aggregate_registry,
            queries: 0,
            expression_subquery: false,
            changes_contributors: false,
            unsupported_terminal_shape: false,
        };
        let _ = model.visit(&mut shape);
        if shape.expression_subquery {
            return Err(unsupported(
                "Tool provenance with policies does not support scalar/expression subqueries",
            ));
        }
        let policies = self.policies();
        if policies.iter().any(|policy| {
            !matches!(
                policy.resolution(),
                Resolution::Kill | Resolution::Remove | Resolution::Retry(_)
            )
        }) {
            return Err(unsupported(
                "Tool provenance cannot repair or recompute external output payloads",
            ));
        }
        let whole_output_remove = shape.changes_contributors
            && policies
                .iter()
                .any(|policy| matches!(policy.resolution(), Resolution::Remove));
        if whole_output_remove && shape.unsupported_terminal_shape {
            return Err(unsupported(
                "Tool provenance REMOVE requires a row-preserving model or a single flat \
                 aggregate/window SELECT; nested, set, distinct and outer-join models \
                 cannot recompute external payloads",
            ));
        }
        Ok(whole_output_remove)
    }

    pub(crate) fn rewrite_tool_publication(
        &self,
        model: &Query,
        publication: &Statement,
        actual_candidates: &Statement,
        options: RewriteOptions,
    ) -> Result<ToolEnforcementPlan, RewriteError> {
        let whole_output_remove = self.validate_tool_model_policy_shape(model)?;
        if !matches!(publication, Statement::Insert(_))
            || !matches!(actual_candidates, Statement::Insert(_))
        {
            return Err(unsupported("Tool publication requires native INSERT plans"));
        }
        if !whole_output_remove {
            return Ok(ToolEnforcementPlan {
                publication_sql: self.rewrite_with_options(&publication.to_string(), options)?,
                sink_kill_preflight_sql: None,
            });
        }

        // A branch view preserves registration identities, aggregate metadata,
        // and catalog semantics. The original policy registry is never mutated.
        let mut terminal = self.tool_policy_branch(false, true);
        // This declaration is scoped to a native-validated flat model followed
        // only by the planner's bijective occurrence binding. It is not a claim
        // that terminal removal can be pushed through arbitrary nested SQL.
        terminal
            .resolution_registry
            .register(PASSANT_REMOVE_OUTPUT, true);
        let sink_kill_preflight_sql = if self.policies().iter().any(|policy| {
            policy.sources().is_empty() && matches!(policy.resolution(), Resolution::Kill)
        }) {
            // Enforce KILL > REMOVE against the original actual outputs without
            // writing them. RETRY deliberately remains below REMOVE precedence.
            let preflight = self.tool_policy_branch(true, false);
            let rewritten =
                preflight.rewrite_with_options(&actual_candidates.to_string(), options.clone())?;
            match parse_query_with_dialect(&rewritten, self.parse_dialect)? {
                Statement::Insert(insert) => Some(
                    insert
                        .source
                        .ok_or_else(|| {
                            unsupported("Tool sink preflight requires an INSERT source")
                        })?
                        .to_string(),
                ),
                _ => return Err(unsupported("Tool sink preflight requires an INSERT source")),
            }
        } else {
            None
        };
        Ok(ToolEnforcementPlan {
            publication_sql: terminal.rewrite_with_options(&publication.to_string(), options)?,
            sink_kill_preflight_sql,
        })
    }

    fn tool_policy_branch(&self, sink_kill_only: bool, terminal_remove: bool) -> Self {
        let entries = self
            .store
            .policy_indices()
            .into_iter()
            .filter_map(|index| {
                let mut policy = self.store.policy(index)?.clone();
                if sink_kill_only
                    && (!policy.sources().is_empty()
                        || !matches!(policy.resolution(), Resolution::Kill))
                {
                    return None;
                }
                if terminal_remove {
                    let PolicyIr::Pgn { on_fail, .. } = &mut policy;
                    if matches!(on_fail, Resolution::Remove) {
                        *on_fail = Resolution::Udf(PASSANT_REMOVE_OUTPUT.to_owned());
                    }
                }
                Some(BranchPolicyEntry {
                    policy,
                    original_policy_index: self.store.original_policy_index(index),
                    constraint_ast: self.store.clone_constraint_ast(index)?,
                })
            })
            .collect();
        let mut branch = Self::with_branch_view(
            &self.store,
            entries,
            self.catalog.clone(),
            self.parse_dialect,
            &self.resolution_registry,
        );
        branch.aggregate_registry = self.aggregate_registry.clone();
        for index in branch.store.policy_indices() {
            branch.finalize_scan_ready_expr(index, &self.aggregate_registry);
        }
        branch
    }
}

fn unsupported(message: &str) -> RewriteError {
    RewriteError::unsupported_statement(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TableCatalog;
    use crate::policy::parse_policy_text;
    use crate::sql::SqlDialect;

    fn statement(sql: &str) -> Statement {
        parse_query_with_dialect(sql, SqlDialect::DuckDb).unwrap()
    }
    fn query(sql: &str) -> Query {
        let Statement::Query(query) = statement(sql) else {
            panic!("expected query")
        };
        *query
    }
    fn rewriter(policy: &str) -> PassantRewriter {
        let mut catalog = TableCatalog::default();
        catalog.register_table("ToolInput", vec!["id".into(), "category".into()]);
        catalog.register_table("ToolOutput", vec!["message".into()]);
        let mut result = PassantRewriter::with_catalog(catalog);
        result
            .register_policy(parse_policy_text(policy).unwrap())
            .unwrap();
        result
    }

    #[test]
    fn native_tool_shape_decisions_reject_nested_contributor_recomputation() {
        let r = rewriter(
            "SOURCE ToolInput SINK ToolOutput CONSTRAINT ToolInput.category='public' ON FAIL REMOVE",
        );
        assert!(
            !r.validate_tool_model_policy_shape(&query("SELECT * FROM ToolInput"))
                .unwrap()
        );
        assert!(
            r.validate_tool_model_policy_shape(&query(
                "SELECT category, COUNT(*) FROM ToolInput GROUP BY category"
            ))
            .unwrap()
        );
        for sql in [
            "SELECT SUM(n) FROM (SELECT COUNT(*) n FROM ToolInput GROUP BY category) grouped",
            "SELECT DISTINCT category FROM ToolInput",
            "SELECT id FROM ToolInput UNION SELECT id FROM ToolInput",
            "SELECT a.id FROM ToolInput a LEFT JOIN ToolInput b ON a.id=b.id",
        ] {
            assert!(
                r.validate_tool_model_policy_shape(&query(sql))
                    .unwrap_err()
                    .to_string()
                    .contains("row-preserving"),
                "{sql}"
            );
        }
        for sql in [
            "SELECT id, (SELECT category FROM ToolInput LIMIT 1) value FROM ToolInput",
            "SELECT id FROM ToolInput WHERE EXISTS (SELECT 1 FROM ToolInput)",
            "SELECT ARRAY(SELECT id FROM ToolInput) FROM ToolInput",
        ] {
            assert!(
                r.validate_tool_model_policy_shape(&query(sql))
                    .unwrap_err()
                    .to_string()
                    .contains("scalar/expression subqueries"),
                "{sql}"
            );
        }
    }

    #[test]
    fn native_tool_enforcement_preserves_original_group_and_policy_registry() {
        let r = rewriter(
            "SOURCE ToolInput SINK ToolOutput CONSTRAINT ToolInput.category='public' ON FAIL REMOVE",
        );
        let model = query("SELECT category, COUNT(*) n FROM ToolInput GROUP BY category");
        let publication = statement(
            "INSERT INTO ToolOutput(message) SELECT actual.v FROM (SELECT category, COUNT(*) n FROM ToolInput GROUP BY category) model JOIN (VALUES ('public','safe'),('private','secret')) actual(k,v) ON model.category=actual.k",
        );
        let candidates =
            statement("INSERT INTO ToolOutput(message) SELECT 'safe' UNION ALL SELECT 'secret'");
        let plan = r
            .rewrite_tool_publication(&model, &publication, &candidates, RewriteOptions::default())
            .unwrap();
        assert!(plan.sink_kill_preflight_sql.is_none());
        assert!(matches!(
            r.policy_by_index(0).unwrap().resolution(),
            Resolution::Remove
        ));
        let db = duckdb::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE ToolInput(id INT,category VARCHAR); INSERT INTO ToolInput VALUES (1,'public'),(2,'private'); CREATE TABLE ToolOutput(message VARCHAR)").unwrap();
        db.execute_batch(&plan.publication_sql).unwrap();
        assert_eq!(
            db.query_row("SELECT message FROM ToolOutput", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "safe"
        );
    }

    #[test]
    fn native_tool_preflight_preserves_sink_kill_over_source_remove() {
        let mut r = rewriter(
            "SOURCE ToolInput SINK ToolOutput CONSTRAINT COUNT(ToolInput.id)=99 ON FAIL REMOVE",
        );
        r.register_policy(
            parse_policy_text(
                "SINK ToolOutput CONSTRAINT ToolOutput.message='allowed' ON FAIL KILL",
            )
            .unwrap(),
        )
        .unwrap();
        let model = query("SELECT COUNT(*) n FROM ToolInput");
        let publication = statement(
            "INSERT INTO ToolOutput(message) SELECT actual.v FROM (SELECT COUNT(*) n FROM ToolInput) model JOIN (VALUES ('denied')) actual(v) ON true",
        );
        let candidates = statement("INSERT INTO ToolOutput(message) SELECT 'denied'");
        let plan = r
            .rewrite_tool_publication(&model, &publication, &candidates, RewriteOptions::default())
            .unwrap();
        let preflight = plan.sink_kill_preflight_sql.unwrap();
        assert!(matches!(statement(&preflight), Statement::Query(_)));
        let db = duckdb::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE ToolInput(id INT,category VARCHAR); INSERT INTO ToolInput VALUES (1,'private'); CREATE TABLE ToolOutput(message VARCHAR); CREATE MACRO passant_kill() AS error('KILLing')").unwrap();
        assert!(
            db.execute_batch(&preflight)
                .unwrap_err()
                .to_string()
                .contains("KILLing")
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM ToolOutput", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn native_tool_branch_preserves_deleted_registration_gaps() {
        let mut r =
            rewriter("SOURCE ToolInput SINK ToolOutput CONSTRAINT ToolInput.id=1 ON FAIL REMOVE");
        let retry = parse_policy_text("SOURCE ToolInput SINK ToolOutput CONSTRAINT ToolInput.id=2 ON FAIL RETRY('retry second')").unwrap();
        r.register_policy(retry.clone()).unwrap();
        assert!(r.delete_policy(None, None, Some("ToolInput.id=1"), None, None));
        let branch = r.tool_policy_branch(false, true);
        assert!(branch.policy_by_index(0).is_none());
        assert_eq!(branch.policy_by_index(1), Some(&retry));
    }
}
