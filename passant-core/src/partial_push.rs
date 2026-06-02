//! Partial-Push rewrite (Section 4): evaluate user aggregates in `base_query` and policy
//! aggregates separately in `policy_eval`, then join the results.

use std::collections::HashSet;

use sqlparser::ast::{BinaryOperator, Expr, Ident, Query, Select, SelectItem, SetExpr, Statement};

use crate::identifiers::TableKey;
use crate::optimizer::RewriteStrategy;
use crate::policy::Resolution;
use crate::rewrite_strategy::{RewriteAttempt, RewriteEngine, RewriteRequest, StatementKind};
use crate::rewriter::{
    PassantRewriter, RewriteError, TableScope, apply_policy_having, ensure_projection_aliases,
    extract_policy_comparison_for_policy, group_by_join_specs, outer_limited_projection_items,
    parse_expr, scope_has_enforcement_policies, select_is_aggregation,
};
use crate::sql::{
    alias_column, and_exprs, column_comparison, cte, empty_select, function_call,
    partial_push_join_from, partial_push_split_query, qualified_column, qualified_wildcard,
    query_from_select, render_statement, statement_from_query, table_factor, with_ctes,
};

const BASE_QUERY_CTE: &str = "base_query";
const POLICY_EVAL_CTE: &str = "policy_eval";
const LIMIT_CTE: &str = "cte";
const PARTIAL_SCAN_CTE: &str = "__passant_partial";
const GLOBAL_PARTIAL_PUSH_KEY: &str = "__passant_partial_push_key";

#[derive(Clone)]
pub(crate) struct ExtraDfcFilter {
    pub alias: String,
    pub subquery_alias: String,
    pub subquery_metric: String,
    pub threshold: String,
    pub op: &'static str,
}

impl std::fmt::Debug for ExtraDfcFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtraDfcFilter")
            .field("alias", &self.alias)
            .field("subquery_alias", &self.subquery_alias)
            .field("subquery_metric", &self.subquery_metric)
            .field("threshold", &self.threshold)
            .field("op", &self.op)
            .finish()
    }
}

/// Partial-push engine — splits user and policy evaluation across CTE boundaries.
pub struct PartialPushEngine;

impl RewriteEngine for PartialPushEngine {
    fn kind(&self) -> RewriteStrategy {
        RewriteStrategy::PartialPush
    }

    fn priority(&self) -> u8 {
        10
    }

    fn matches(&self, rewriter: &PassantRewriter, request: &RewriteRequest<'_>) -> bool {
        if !rewriter.has_registered_policies() {
            return false;
        }
        if matches!(request.kind, StatementKind::Passthrough) {
            return false;
        }
        request.requires_partial_push()
    }

    fn rewrite(
        &self,
        rewriter: &PassantRewriter,
        request: &RewriteRequest<'_>,
    ) -> Result<RewriteAttempt, RewriteError> {
        if request.semiring.all_distributive && !request.options.use_partial_push {
            return Ok(RewriteAttempt::Skipped);
        }

        let Statement::Query(query) = request.statement else {
            return Ok(RewriteAttempt::Skipped);
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            return Ok(RewriteAttempt::Skipped);
        };

        if request.kind != StatementKind::SelectQuery {
            let mut statement = request.statement.clone();
            rewriter.rewrite_statement_full_push(&mut statement, &request.options)?;
            return Ok(RewriteAttempt::Applied(crate::sql::render_statement(
                &statement, None,
            )));
        }

        if !has_applicable_enforcement_policies(rewriter, request, select) {
            return Ok(RewriteAttempt::Skipped);
        }

        let is_aggregation = request
            .analysis
            .select_scopes
            .first()
            .map(|scope| scope.is_aggregation)
            .unwrap_or_else(|| select_is_aggregation(select, &rewriter.aggregate_registry));
        let has_limit = request
            .select
            .as_ref()
            .map(|shape| shape.has_limit)
            .unwrap_or_else(|| query_has_limit(query));
        let has_limit_policy =
            has_limit && has_applicable_enforcement_policies(rewriter, request, select);

        let rewritten = if has_limit_policy {
            if let Some(sql) = crate::rewriter::limit::try_render_limited_policy_wrapper(
                rewriter,
                request.statement,
                PARTIAL_SCAN_CTE,
            )? {
                sql
            } else if is_aggregation {
                partial_push_limit_aggregation(rewriter, request.statement)?
            } else {
                crate::rewriter::limit::render_limited_policy_wrapper(
                    rewriter,
                    request.statement,
                    PARTIAL_SCAN_CTE,
                )?
            }
        } else if is_aggregation {
            partial_push_aggregation(rewriter, request.statement)?
        } else {
            partial_push_scan(rewriter, request.statement)?
        };

        Ok(RewriteAttempt::Applied(rewritten))
    }
}

fn has_applicable_enforcement_policies(
    rewriter: &PassantRewriter,
    request: &RewriteRequest<'_>,
    select: &Select,
) -> bool {
    let main_tables = request
        .analysis
        .select_scopes
        .first()
        .map(|scope| scope.scope.direct_base_tables.clone())
        .unwrap_or_default();
    scope_has_enforcement_policies(
        rewriter.policy_store(),
        &main_tables,
        &exists_subquery_policy_tables(select),
    )
}

fn exists_subquery_policy_tables(select: &Select) -> HashSet<TableKey> {
    let Some(where_expr) = &select.selection else {
        return HashSet::new();
    };
    let mut tables = HashSet::new();
    for conjunct in flatten_and(where_expr) {
        let subquery = match &conjunct {
            Expr::Exists {
                subquery,
                negated: false,
            } => Some(subquery),
            Expr::InSubquery {
                subquery,
                negated: false,
                ..
            } => Some(subquery),
            _ => None,
        };
        let Some(subquery) = subquery else {
            continue;
        };
        let SetExpr::Select(exists_select) = subquery.body.as_ref() else {
            continue;
        };
        tables.extend(select_direct_base_tables(exists_select));
    }
    tables
}

fn select_direct_base_tables(select: &Select) -> HashSet<TableKey> {
    TableScope::from_select(select).direct_base_tables
}

fn flatten_and(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut parts = flatten_and(left);
            parts.extend(flatten_and(right));
            parts
        }
        other => vec![other.clone()],
    }
}

fn partial_push_aggregation(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::unsupported_statement(
            "partial-push requires a SELECT query",
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "partial-push requires a SELECT body",
        ));
    };

    let mut base_query = *query.clone();
    if let SetExpr::Select(base_select) = base_query.body.as_mut() {
        ensure_projection_aliases(base_select);
    }

    let group_specs = group_by_join_specs(select)?;
    let (policy_eval_query, join_keys, _extra_dfc) =
        build_policy_eval_query(rewriter, select, &group_specs, false, None)?;

    Ok(render_statement(
        &statement_from_query(partial_push_split_query(
            base_query,
            policy_eval_query,
            &join_keys,
        )),
        None,
    ))
}

fn partial_push_limit_aggregation(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::unsupported_statement(
            "partial-push LIMIT rewrite requires a SELECT query",
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "partial-push LIMIT rewrite requires a SELECT body",
        ));
    };
    let table_scope = TableScope::from_select(select);
    let (remove_index, remove_policy) = rewriter
        .policy_store()
        .candidate_ids_for_tables(&table_scope.direct_base_tables)
        .into_iter()
        .find_map(|index| {
            let policy = rewriter.policy_at(index)?;
            (policy.resolution() == Resolution::Remove).then_some((index, policy))
        })
        .ok_or_else(|| {
            RewriteError::unsupported_statement(
                "partial-push LIMIT rewrite requires a REMOVE policy",
            )
        })?;

    let mut base_query = *query.clone();
    if let SetExpr::Select(base_select) = base_query.body.as_mut() {
        ensure_projection_aliases(base_select);
    }

    let group_specs = group_by_join_specs(select)?;
    let (left, threshold, op) = extract_policy_comparison_for_policy(
        rewriter.policy_store(),
        remove_index,
        remove_policy.constraint(),
        None,
    )?;
    let constraint_expr =
        rewriter
            .policy_store()
            .constraint_expr(remove_index, remove_policy.constraint(), None)?;
    let dfc_expr = match constraint_expr {
        Expr::BinaryOp { left, .. } => *left,
        _ => parse_expr(&left)?,
    };

    let (policy_eval_query, join_keys, extra_dfc) = build_policy_eval_query(
        rewriter,
        select,
        &group_specs,
        true,
        Some((dfc_expr, "dfc")),
    )?;

    let mut limit_select = empty_select();
    limit_select.projection = vec![
        qualified_wildcard(BASE_QUERY_CTE),
        alias_column(POLICY_EVAL_CTE, "dfc", "dfc"),
    ];
    for extra in &extra_dfc {
        limit_select
            .projection
            .push(alias_column(POLICY_EVAL_CTE, &extra.alias, &extra.alias));
    }
    limit_select.from = vec![partial_push_join_from(
        BASE_QUERY_CTE,
        POLICY_EVAL_CTE,
        &join_keys,
    )];

    let mut limit_query = query_from_select(limit_select);
    limit_query.order_by = query.order_by.clone();
    limit_query.limit = query.limit.clone();
    limit_query.offset = query.offset.clone();
    limit_query.fetch = query.fetch.clone();

    let mut projection_select = select.clone();
    ensure_projection_aliases(&mut projection_select);
    let outer_projection = outer_limited_projection_items(&projection_select);

    let mut where_exprs = vec![
        column_comparison("dfc", op, parse_expr(&threshold)?).ok_or_else(|| {
            RewriteError::unsupported_statement(
                "partial-push LIMIT rewrite uses unsupported comparison operator",
            )
        })?,
    ];
    for extra in &extra_dfc {
        where_exprs.push(
            column_comparison(&extra.alias, extra.op, parse_expr(&extra.threshold)?).ok_or_else(
                || {
                    RewriteError::unsupported_statement(
                        "partial-push LIMIT rewrite uses unsupported comparison operator",
                    )
                },
            )?,
        );
    }

    let mut outer_select = empty_select();
    outer_select.projection = outer_projection;
    outer_select.from = vec![sqlparser::ast::TableWithJoins {
        relation: table_factor(LIMIT_CTE),
        joins: Vec::new(),
    }];
    outer_select.selection = and_exprs(where_exprs);

    Ok(render_statement(
        &statement_from_query(with_ctes(
            vec![
                cte(BASE_QUERY_CTE, base_query),
                cte(POLICY_EVAL_CTE, policy_eval_query),
                cte(LIMIT_CTE, limit_query),
            ],
            SetExpr::Select(Box::new(outer_select)),
        )),
        None,
    ))
}

fn partial_push_scan(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::unsupported_statement(
            "partial-push scan rewrite requires a SELECT query",
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "partial-push scan rewrite requires a SELECT body",
        ));
    };

    let scan_keys = group_by_join_specs(select).unwrap_or_default();
    let (policy_eval_query, join_keys, _) =
        build_policy_eval_query(rewriter, select, &scan_keys, false, None)?;
    let base_query = match statement {
        Statement::Query(query) => *query.clone(),
        _ => unreachable!("validated above"),
    };
    Ok(render_statement(
        &statement_from_query(partial_push_split_query(
            base_query,
            policy_eval_query,
            &join_keys,
        )),
        None,
    ))
}

fn build_policy_eval_query(
    rewriter: &PassantRewriter,
    select: &Select,
    group_specs: &[(String, Expr)],
    include_primary_dfc: bool,
    primary_dfc: Option<(Expr, &str)>,
) -> Result<(Query, Vec<String>, Vec<ExtraDfcFilter>), RewriteError> {
    let mut policy_eval = select.clone();
    policy_eval.having = None;

    let exists_handled = rewriter.rewrite_exists_subqueries_as_joins(&mut policy_eval)?;
    let (in_handled, extra_dfc) = rewriter.rewrite_in_subqueries_as_joins(&mut policy_eval)?;

    let join_keys: Vec<String> = if group_specs.is_empty() {
        policy_eval.projection = vec![SelectItem::ExprWithAlias {
            expr: parse_expr("1")?,
            alias: Ident::new(GLOBAL_PARTIAL_PUSH_KEY),
        }];
        Vec::new()
    } else {
        policy_eval.projection = group_specs
            .iter()
            .map(|(key_name, key_expr)| SelectItem::ExprWithAlias {
                expr: key_expr.clone(),
                alias: Ident::new(key_name),
            })
            .collect();
        group_specs.iter().map(|(name, _)| name.clone()).collect()
    };

    if include_primary_dfc && let Some((expr, alias)) = primary_dfc {
        policy_eval.projection.push(SelectItem::ExprWithAlias {
            expr,
            alias: Ident::new(alias),
        });
    }

    for extra in &extra_dfc {
        policy_eval.projection.push(SelectItem::ExprWithAlias {
            expr: function_call(
                "max",
                vec![qualified_column(
                    &extra.subquery_alias,
                    &extra.subquery_metric,
                )],
            ),
            alias: Ident::new(&extra.alias),
        });
    }

    let mut skip = exists_handled;
    skip.extend(in_handled);
    apply_policy_having(rewriter, &mut policy_eval, &skip)?;

    Ok((query_from_select(policy_eval), join_keys, extra_dfc))
}

fn query_has_limit(query: &Query) -> bool {
    query.limit.is_some() || query.offset.is_some() || query.fetch.is_some()
}
