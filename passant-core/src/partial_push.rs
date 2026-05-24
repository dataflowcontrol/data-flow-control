//! Partial-Push rewrite (Section 4): evaluate user aggregates in `base_query` and policy
//! aggregates separately in `policy_eval`, then join the results.

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{BinaryOperator, Expr, Ident, Query, Select, SelectItem, SetExpr, Statement};

use crate::optimizer::RewriteStrategy;
use crate::policy::{PolicyIr, Resolution};
use crate::rewrite_strategy::{RewriteAttempt, RewriteEngine, RewriteRequest, StatementKind};
use crate::rewriter::{
    PassantRewriter, RewriteContext, RewriteError, TableScope, apply_policy_having,
    build_compat_dfc_filter_expr, collect_compound_columns_by_name, ensure_projection_aliases,
    extract_policy_comparison, group_by_join_specs, kill_expr, outer_limited_projection,
    parse_expr, policy_applicability, projected_column_name, replace_identifiers, resolver_expr,
    select_is_aggregation, unqualify_columns,
};

const BASE_QUERY_CTE: &str = "base_query";
const POLICY_EVAL_CTE: &str = "policy_eval";
const LIMIT_CTE: &str = "cte";
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
        if rewriter.policies().is_empty() {
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
            rewriter.rewrite_statement_full_push(&mut statement)?;
            return Ok(RewriteAttempt::Applied(statement.to_string()));
        }

        if !has_applicable_enforcement_policies(rewriter, select) {
            return Ok(RewriteAttempt::Skipped);
        }

        let is_aggregation = select_is_aggregation(select);
        let has_limit = query_limit_clause(query).is_some();
        let has_remove = has_remove_enforcement_policy(rewriter);

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
    let table_scope = TableScope::from_select(select);
    let main_tables = &table_scope.direct_base_tables;
    let exists_subquery_tables = exists_subquery_policy_tables(select);

    rewriter.policies().iter().any(|policy| {
        if !matches!(
            policy.resolution(),
            Resolution::Remove | Resolution::Kill | Resolution::Llm
        ) {
            return false;
        }
        if policy_applicability(policy, main_tables, None, false).is_some() {
            return true;
        }
        policy.sources().iter().any(|source| {
            let key = source.to_ascii_lowercase();
            exists_subquery_tables.contains(&key) && !main_tables.contains(&key)
        })
    })
}

fn exists_subquery_policy_tables(select: &Select) -> HashSet<String> {
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

fn select_direct_base_tables(select: &Select) -> HashSet<String> {
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

fn has_remove_enforcement_policy(rewriter: &PassantRewriter) -> bool {
    rewriter
        .policies()
        .iter()
        .any(|policy| policy.resolution() == Resolution::Remove)
}

fn partial_push_aggregation(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::Unsupported(
            "partial-push requires a SELECT query".into(),
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::Unsupported(
            "partial-push requires a SELECT body".into(),
        ));
    };

    let mut base_query = query.clone();
    if let SetExpr::Select(base_select) = base_query.body.as_mut() {
        ensure_projection_aliases(base_select);
    }

    let group_specs = group_by_join_specs(select)?;
    let (policy_eval_sql, join_keys, _extra_dfc) =
        build_policy_eval_sql(rewriter, select, &group_specs, false, None)?;

    let base_sql = Statement::Query(base_query).to_string();
    let join_clause = partial_push_join_clause(&join_keys);
    Ok(format!(
        "WITH {BASE_QUERY_CTE} AS ({base_sql}), {POLICY_EVAL_CTE} AS ({policy_eval_sql}) SELECT {BASE_QUERY_CTE}.* {join_clause}"
    ))
}

fn partial_push_limit_aggregation(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::Unsupported(
            "partial-push LIMIT rewrite requires a SELECT query".into(),
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::Unsupported(
            "partial-push LIMIT rewrite requires a SELECT body".into(),
        ));
    };
    let remove_policy = rewriter
        .policies()
        .iter()
        .find(|policy| policy.resolution() == Resolution::Remove)
        .ok_or_else(|| {
            RewriteError::Unsupported("partial-push LIMIT rewrite requires a REMOVE policy".into())
        })?;

    let mut base_query = query.clone();
    if let SetExpr::Select(base_select) = base_query.body.as_mut() {
        ensure_projection_aliases(base_select);
    }

    let group_specs = group_by_join_specs(select)?;
    let (left, threshold, op) = extract_policy_comparison(remove_policy.constraint())?;
    let dfc_projection = format!("{left} AS dfc");

    let (policy_eval_sql, join_keys, extra_dfc) = build_policy_eval_sql(
        rewriter,
        select,
        &group_specs,
        true,
        Some(dfc_projection.as_str()),
    )?;

    let base_sql = Statement::Query(base_query).to_string();
    let join_clause = partial_push_join_clause(&join_keys);

    let mut cte_select = format!("SELECT {BASE_QUERY_CTE}.*, {POLICY_EVAL_CTE}.dfc AS dfc");
    for extra in &extra_dfc {
        cte_select.push_str(&format!(
            ", {POLICY_EVAL_CTE}.{} AS {}",
            extra.alias, extra.alias
        ));
    }
    cte_select.push(' ');
    cte_select.push_str(&join_clause);

    if let Some(order_by) = &query.order_by {
        cte_select.push_str(&format!(" {order_by}"));
    }
    if let Some(limit) = query_limit_clause(query) {
        cte_select.push_str(&format!(" {limit}"));
    }

    let mut projection_select = select.clone();
    ensure_projection_aliases(&mut projection_select);
    let outer_projection = outer_limited_projection(&projection_select);
    let mut where_parts = vec![format!("dfc {op} {threshold}")];
    for extra in &extra_dfc {
        where_parts.push(format!("{} {} {}", extra.alias, extra.op, extra.threshold));
    }

    Ok(format!(
        "WITH {BASE_QUERY_CTE} AS ({base_sql}), {POLICY_EVAL_CTE} AS ({policy_eval_sql}), {LIMIT_CTE} AS ({cte_select}) SELECT {outer_projection} FROM {LIMIT_CTE} WHERE {}",
        where_parts.join(" AND ")
    ))
}

fn partial_push_limit_scan(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::Unsupported(
            "partial-push LIMIT scan rewrite requires a SELECT query".into(),
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::Unsupported(
            "partial-push LIMIT scan rewrite requires a SELECT body".into(),
        ));
    };

    let table_scope = TableScope::from_select(select);
    let mut filters = Vec::new();
    let mut propagated_filter_columns = HashMap::new();
    let projected_names = projected_select_names(select);

    for (policy, applicability) in rewriter.policies().iter().filter_map(|policy| {
        policy_applicability(policy, &table_scope.direct_base_tables, None, false)
            .map(|applicability| (policy, applicability))
    }) {
        let PolicyIr::CompatDfc {
            constraint,
            on_fail,
            ..
        } = policy
        else {
            continue;
        };
        if !matches!(
            on_fail,
            Resolution::Remove | Resolution::Kill | Resolution::Llm
        ) {
            continue;
        }
        let mut expr = build_compat_dfc_filter_expr(
            policy.sources(),
            constraint,
            &match policy {
                PolicyIr::CompatDfc { sink_alias, .. } => sink_alias.clone(),
                _ => None,
            },
            applicability,
            &RewriteContext::default(),
            &table_scope,
            false,
        )?;
        let mut source_columns = HashMap::new();
        collect_compound_columns_by_name(&expr, &mut source_columns);
        unqualify_columns(&mut expr);
        for (name, source_expr) in source_columns {
            if !projected_names.contains(&name) {
                let alias = format!("__passant_filter_{name}");
                propagated_filter_columns
                    .entry(name)
                    .or_insert((source_expr, alias));
            }
        }
        if *on_fail == Resolution::Kill {
            expr = kill_expr(expr)?;
        } else if *on_fail == Resolution::Llm {
            expr = resolver_expr(expr)?;
        }
        let replacements = propagated_filter_columns
            .iter()
            .map(|(name, (_, alias))| (name.clone(), alias.clone()))
            .collect::<HashMap<_, _>>();
        replace_identifiers(&mut expr, &replacements);
        filters.push(expr.to_string());
    }

    if filters.is_empty() {
        return Err(RewriteError::Unsupported(
            "partial-push LIMIT scan rewrite found no applicable filters".into(),
        ));
    }

    let mut inner = query.clone();
    let mut outer_projection = outer_limited_projection(select);
    if let SetExpr::Select(inner_select) = inner.body.as_mut() {
        ensure_projection_aliases(inner_select);
        for (_, (expr, alias)) in propagated_filter_columns {
            inner_select.projection.push(SelectItem::ExprWithAlias {
                expr,
                alias: Ident::new(alias),
            });
        }
        outer_projection = outer_limited_projection(inner_select);
    }

    Ok(format!(
        "WITH __passant_partial AS ({}) SELECT {outer_projection} FROM __passant_partial WHERE {}",
        Statement::Query(inner),
        filters.join(" AND ")
    ))
}

fn partial_push_scan(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<String, RewriteError> {
    let Statement::Query(query) = statement else {
        return Err(RewriteError::Unsupported(
            "partial-push scan rewrite requires a SELECT query".into(),
        ));
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(RewriteError::Unsupported(
            "partial-push scan rewrite requires a SELECT body".into(),
        ));
    };

    let scan_keys = group_by_join_specs(select).unwrap_or_default();
    let (policy_eval_sql, join_keys, _) =
        build_policy_eval_sql(rewriter, select, &scan_keys, false, None)?;
    let base_sql = statement.to_string();
    let join_clause = partial_push_join_clause(&join_keys);
    Ok(format!(
        "WITH {BASE_QUERY_CTE} AS ({base_sql}), {POLICY_EVAL_CTE} AS ({policy_eval_sql}) SELECT {BASE_QUERY_CTE}.* {join_clause}"
    ))
}

fn build_policy_eval_sql(
    rewriter: &PassantRewriter,
    select: &Select,
    group_specs: &[(String, Expr)],
    include_primary_dfc: bool,
    primary_dfc_projection: Option<&str>,
) -> Result<(String, Vec<String>, Vec<ExtraDfcFilter>), RewriteError> {
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

    if include_primary_dfc {
        if let Some(projection) = primary_dfc_projection {
            let expr = parse_expr(projection.split(" AS ").next().unwrap_or(projection))?;
            policy_eval.projection.push(SelectItem::ExprWithAlias {
                expr,
                alias: Ident::new("dfc"),
            });
        }
    }

    for extra in &extra_dfc {
        policy_eval.projection.push(SelectItem::ExprWithAlias {
            expr: parse_expr(&format!(
                "max({}.{})",
                extra.subquery_alias, extra.subquery_metric
            ))?,
            alias: Ident::new(&extra.alias),
        });
    }

    let mut skip = exists_handled;
    skip.extend(in_handled);
    apply_policy_having(rewriter, &mut policy_eval, &skip)?;

    Ok((policy_eval.to_string(), join_keys, extra_dfc))
}

fn partial_push_join_clause(key_names: &[String]) -> String {
    if key_names.is_empty() {
        return format!("FROM {BASE_QUERY_CTE} CROSS JOIN {POLICY_EVAL_CTE}");
    }
    let conditions = key_names
        .iter()
        .map(|key| format!("{BASE_QUERY_CTE}.{key} = {POLICY_EVAL_CTE}.{key}"))
        .collect::<Vec<_>>()
        .join(" AND ");
    format!("FROM {BASE_QUERY_CTE} JOIN {POLICY_EVAL_CTE} ON {conditions}")
}

fn query_limit_clause(query: &Query) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(limit) = &query.limit {
        parts.push(format!("LIMIT {limit}"));
    }
    if let Some(offset) = &query.offset {
        parts.push(format!("OFFSET {offset}"));
    }
    if let Some(fetch) = &query.fetch {
        parts.push(fetch.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
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
