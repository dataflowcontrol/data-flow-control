//! Partial-Push rewrite (Section 4): evaluate user aggregates in `base_query` and policy
//! aggregates separately in `policy_eval`, then join the results.

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{BinaryOperator, Expr, Ident, Query, Select, SelectItem, SetExpr, Statement};

use crate::identifiers::TableKey;
use crate::optimizer::RewriteStrategy;
use crate::policy::Resolution;
use crate::query_analysis::SelectAnalysis;
use crate::rewrite_strategy::{RewriteAttempt, RewriteEngine, RewriteRequest, StatementKind};
use crate::rewriter::{
    PassantRewriter, PolicyResolutionAction, RewriteContext, RewriteError, TableScope,
    apply_policy_having, collect_compound_columns_by_name, ensure_projection_aliases,
    extract_policy_comparison_for_policy, group_by_join_specs, kill_expr,
    outer_limited_projection_items, parse_expr, plan_policy_filter_actions, projected_column_name,
    replace_identifiers, resolver_expr, scope_has_enforcement_policies, select_is_aggregation,
    unqualify_columns,
};
use crate::sql::{
    alias_column, and_exprs, column_comparison, cte, empty_select, function_call,
    partial_push_join_from, partial_push_split_query, passant_filter_temp_column, qualified_column,
    qualified_wildcard, query_from_select, statement_from_query, table_factor, with_ctes,
};

const BASE_QUERY_CTE: &str = "base_query";
const POLICY_EVAL_CTE: &str = "policy_eval";
const LIMIT_CTE: &str = "cte";
const PARTIAL_SCAN_CTE: &str = "__passant_partial";
const GLOBAL_PARTIAL_PUSH_KEY: &str = "__passant_partial_push_key";

pub(crate) struct ExtraDfcFilter {
    pub alias: String,
    pub subquery_alias: String,
    pub subquery_metric: String,
    pub threshold: String,
    pub op: &'static str,
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
            rewriter.rewrite_statement_full_push(&mut statement, request.options.collect_stats)?;
            return Ok(RewriteAttempt::Applied(statement.to_string()));
        }

        if !has_applicable_enforcement_policies(rewriter, select) {
            return Ok(RewriteAttempt::Skipped);
        }

        let is_aggregation = select_is_aggregation(select);
        let has_limit = query_has_limit(query);
        let has_remove = select_has_applicable_remove_policy(rewriter, select);

        let rewritten = if has_limit && has_remove {
            if is_aggregation {
                partial_push_limit_aggregation(rewriter, request.statement)?
            } else {
                partial_push_limit_scan(rewriter, request.statement)?
            }
        } else if is_aggregation {
            partial_push_aggregation(rewriter, request.statement)?
        } else {
            partial_push_scan(rewriter, request.statement)?
        };

        Ok(RewriteAttempt::Applied(rewritten))
    }
}

fn has_applicable_enforcement_policies(rewriter: &PassantRewriter, select: &Select) -> bool {
    let analysis = SelectAnalysis::from_select(select);
    let main_tables = &analysis.scope.direct_base_tables;
    scope_has_enforcement_policies(
        rewriter.policy_store(),
        main_tables,
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
    SelectAnalysis::from_select(select).scope.direct_base_tables
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

fn select_has_applicable_remove_policy(rewriter: &PassantRewriter, select: &Select) -> bool {
    let analysis = SelectAnalysis::from_select(select);
    let tables = &analysis.scope.direct_base_tables;
    rewriter
        .policy_store()
        .candidate_scope_lookup(tables, None, crate::MultiSourceLookupMode::AnyOverlap)
        .iter()
        .any(|index| {
            rewriter
                .policy_at(index)
                .is_some_and(|policy| policy.resolution() == Resolution::Remove)
        })
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

    Ok(statement_from_query(partial_push_split_query(
        base_query,
        policy_eval_query,
        &join_keys,
    ))
    .to_string())
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

    Ok(statement_from_query(with_ctes(
        vec![
            cte(BASE_QUERY_CTE, base_query),
            cte(POLICY_EVAL_CTE, policy_eval_query),
            cte(LIMIT_CTE, limit_query),
        ],
        SetExpr::Select(Box::new(outer_select)),
    ))
    .to_string())
}

fn partial_push_limit_scan(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::unsupported_statement(
            "partial-push LIMIT scan rewrite requires a SELECT query",
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "partial-push LIMIT scan rewrite requires a SELECT body",
        ));
    };

    let table_scope = TableScope::from_select(select);
    let mut filters = Vec::new();
    let mut propagated_filter_columns = HashMap::new();
    let projected_names = projected_select_names(select);

    let context = RewriteContext::default();
    let (actions, _) = plan_policy_filter_actions(
        rewriter.policy_store(),
        rewriter.catalog(),
        None,
        &table_scope.direct_base_tables,
        &table_scope,
        None,
        &context,
        false,
        &HashSet::new(),
        &HashSet::new(),
    )?;

    for action in actions {
        let PolicyResolutionAction::CompatDfc {
            filter: mut expr,
            on_fail,
            ..
        } = action
        else {
            continue;
        };
        if !matches!(
            on_fail,
            Resolution::Remove | Resolution::Kill | Resolution::Llm
        ) {
            continue;
        }
        let mut source_columns = HashMap::new();
        collect_compound_columns_by_name(&expr, &mut source_columns);
        unqualify_columns(&mut expr);
        for (name, source_expr) in source_columns {
            if !projected_names.contains(&name) {
                let alias = passant_filter_temp_column(&name);
                propagated_filter_columns
                    .entry(name)
                    .or_insert((source_expr, alias));
            }
        }
        if on_fail == Resolution::Kill {
            expr = kill_expr(expr)?;
        } else if on_fail == Resolution::Llm {
            expr = resolver_expr(expr)?;
        }
        let replacements = propagated_filter_columns
            .iter()
            .map(|(name, (_, alias))| (name.clone(), alias.clone()))
            .collect::<HashMap<_, _>>();
        replace_identifiers(&mut expr, &replacements);
        filters.push(expr);
    }

    if filters.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "partial-push LIMIT scan rewrite found no applicable filters",
        ));
    }

    let mut inner = *query.clone();
    if let SetExpr::Select(inner_select) = inner.body.as_mut() {
        ensure_projection_aliases(inner_select);
        for (_, (expr, alias)) in propagated_filter_columns {
            inner_select.projection.push(SelectItem::ExprWithAlias {
                expr,
                alias: Ident::new(alias),
            });
        }
    }

    let outer_projection = if let SetExpr::Select(inner_select) = inner.body.as_ref() {
        outer_limited_projection_items(inner_select)
    } else {
        outer_limited_projection_items(select)
    };

    let mut outer_select = empty_select();
    outer_select.projection = outer_projection;
    outer_select.from = vec![sqlparser::ast::TableWithJoins {
        relation: table_factor(PARTIAL_SCAN_CTE),
        joins: Vec::new(),
    }];
    outer_select.selection = and_exprs(filters);

    Ok(statement_from_query(with_ctes(
        vec![cte(PARTIAL_SCAN_CTE, inner)],
        SetExpr::Select(Box::new(outer_select)),
    ))
    .to_string())
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
    Ok(statement_from_query(partial_push_split_query(
        base_query,
        policy_eval_query,
        &join_keys,
    ))
    .to_string())
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

fn projected_select_names(select: &Select) -> HashSet<String> {
    select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::UnnamedExpr(expr) => projected_column_name(expr),
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            _ => None,
        })
        .map(|name| name.to_ascii_lowercase())
        .collect()
}
