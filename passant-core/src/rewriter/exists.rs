use std::collections::HashSet;

use sqlparser::ast::{
    BinaryOperator, Expr, Ident, Join, JoinConstraint, JoinOperator, Select, SelectItem, SetExpr,
    Statement, TableAlias, TableFactor,
};

use crate::identifiers::{TableKey, column_name_from_expr, table_name_from_column_expr};
use crate::parser::parse_query;
use crate::partial_push::ExtraDfcFilter;
use crate::policy::{PolicyIr, Resolution};
use crate::sql::{
    alias_expr, binary_comparison, function_call, grouped_select, qualified_column,
    query_from_select, unqualify_table_refs,
};

use super::RewriteError;
use super::expr::{
    add_filter, first_function_expr, is_aggregate_name, parse_expr, projection_expr_and_name,
};
use super::helpers::{flatten_and, rebuild_and};
use super::projection::extract_policy_comparison;
use super::scope::TableScope;
use super::types::PassantRewriter;

fn select_direct_base_tables(select: &Select) -> HashSet<TableKey> {
    TableScope::from_select(select).direct_base_tables
}

fn column_matches_policy_table(column: &Expr, policy_table: &str) -> bool {
    if let Some(table) = table_name_from_column_expr(column) {
        return table.matches_name(policy_table);
    }
    let Some(name) = column_name_from_expr(column) else {
        return false;
    };
    let prefix = TableKey::single_char_prefix(policy_table);
    name.key().starts_with(&prefix)
}

fn extract_exists_join_columns(where_expr: &Expr, policy_table: &str) -> Option<(String, String)> {
    for conjunct in flatten_and(where_expr) {
        let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = conjunct
        else {
            continue;
        };
        let left_is_subquery = column_matches_policy_table(left.as_ref(), policy_table);
        let right_is_subquery = column_matches_policy_table(right.as_ref(), policy_table);
        if left_is_subquery && !right_is_subquery {
            return Some((right.to_string(), left.to_string()));
        }
        if right_is_subquery && !left_is_subquery {
            return Some((left.to_string(), right.to_string()));
        }
    }
    None
}

fn remove_join_equality_from_where(
    where_expr: &Expr,
    subquery_col: &str,
    outer_col: &str,
) -> Option<Expr> {
    let remaining = flatten_and(where_expr)
        .into_iter()
        .filter(|conjunct| {
            !matches!(conjunct, Expr::BinaryOp { left, op: BinaryOperator::Eq, right }
                if left.to_string() == subquery_col && right.to_string() == outer_col
                    || left.to_string() == outer_col && right.to_string() == subquery_col)
        })
        .collect::<Vec<_>>();
    rebuild_and(remaining)
}

fn exists_subquery_aggregate_projection(
    constraint: &str,
    policy_table: &str,
) -> Result<(SelectItem, String), RewriteError> {
    let expr = parse_expr(constraint)?;
    let Expr::BinaryOp { left, .. } = expr else {
        return Err(RewriteError::unsupported_statement(
            "EXISTS policy constraint must be a comparison",
        ));
    };
    let Expr::Function(function) = left.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "EXISTS policy constraint must use an aggregate",
        ));
    };
    if !is_aggregate_name(&function.name.to_string()) {
        return Err(RewriteError::unsupported_statement(
            "EXISTS policy constraint must use an aggregate",
        ));
    }
    let unqualified = unqualify_table_refs(Expr::Function(function.clone()), policy_table);
    Ok((alias_expr(unqualified, "agg_0"), "agg_0".to_string()))
}

fn build_exists_join_subquery_sql(
    policy_table: &str,
    join_key_col: &str,
    other_where: Option<&Expr>,
    agg_projection: &SelectItem,
    _agg_alias: &str,
) -> Result<String, RewriteError> {
    let join_key_expr = parse_expr(join_key_col)?;
    let join_key_name = column_name_from_expr(&join_key_expr)
        .map(|name| name.as_str().to_string())
        .unwrap_or_else(|| join_key_expr.to_string());
    let projection = vec![
        alias_expr(join_key_expr.clone(), &join_key_name),
        agg_projection.clone(),
    ];
    let select = grouped_select(
        projection,
        vec![sqlparser::ast::TableWithJoins {
            relation: TableFactor::Table {
                name: sqlparser::ast::ObjectName(vec![Ident::new(policy_table)]),
                alias: None,
                args: None,
                with_hints: Vec::new(),
                version: None,
                with_ordinality: false,
                partitions: Vec::new(),
                json_path: None,
            },
            joins: Vec::new(),
        }],
        other_where.cloned(),
        vec![parse_expr(join_key_col)?],
    );
    Ok(query_from_select(select).to_string())
}

fn exists_subquery_having_expr(constraint: &str, agg_alias: &str) -> Result<Expr, RewriteError> {
    let expr = parse_expr(constraint)?;
    let Expr::BinaryOp {
        mut left,
        op,
        right,
    } = expr
    else {
        return Err(RewriteError::unsupported_statement(
            "EXISTS policy constraint must be a comparison",
        ));
    };
    replace_aggregate_with_exists_subquery(&mut left, agg_alias);
    Ok(Expr::BinaryOp { left, op, right })
}

fn replace_aggregate_with_exists_subquery(expr: &mut Expr, agg_alias: &str) {
    match expr {
        Expr::Function(function) if is_aggregate_name(&function.name.to_string()) => {
            *expr = function_call("max", vec![qualified_column("exists_subquery", agg_alias)]);
        }
        Expr::BinaryOp { left, right, .. } => {
            replace_aggregate_with_exists_subquery(left, agg_alias);
            replace_aggregate_with_exists_subquery(right, agg_alias);
        }
        Expr::Nested(inner) => replace_aggregate_with_exists_subquery(inner, agg_alias),
        _ => {}
    }
}

impl PassantRewriter {
    pub(crate) fn rewrite_in_subqueries_as_joins_impl(
        &self,
        select: &mut Select,
    ) -> Result<(HashSet<usize>, Vec<ExtraDfcFilter>), RewriteError> {
        let mut handled = HashSet::new();
        let mut extra_dfc = Vec::new();
        let Some(where_expr) = select.selection.take() else {
            return Ok((handled, extra_dfc));
        };
        let conjuncts = flatten_and(&where_expr);
        let mut remaining = Vec::new();
        let main_tables = TableScope::from_select(select).direct_base_tables;

        for conjunct in conjuncts {
            let Expr::InSubquery {
                expr,
                subquery,
                negated,
            } = &conjunct
            else {
                remaining.push(conjunct);
                continue;
            };
            if *negated {
                remaining.push(conjunct);
                continue;
            }

            let SetExpr::Select(in_select) = subquery.body.as_ref() else {
                remaining.push(conjunct);
                continue;
            };
            let subquery_tables = select_direct_base_tables(in_select);
            let Some((policy_index, constraint)) =
                self.policies
                    .iter()
                    .enumerate()
                    .find_map(|(index, policy)| {
                        let PolicyIr::CompatDfc {
                            sources,
                            constraint,
                            on_fail: Resolution::Remove,
                            ..
                        } = policy
                        else {
                            return None;
                        };
                        if !sources.iter().any(|source| {
                            let key = TableKey::new(source);
                            subquery_tables.contains(&key) && !main_tables.contains(&key)
                        }) {
                            return None;
                        }
                        Some((index, constraint.clone()))
                    })
            else {
                remaining.push(conjunct);
                continue;
            };

            let Some(join_key_col) = in_select
                .projection
                .first()
                .and_then(|item| projection_expr_and_name(item).map(|(expr, _)| expr.to_string()))
            else {
                remaining.push(conjunct);
                continue;
            };
            let join_key_name = column_name_from_expr(&parse_expr(&join_key_col)?)
                .map(|name| name.as_str().to_string())
                .unwrap_or_else(|| join_key_col.clone());
            let join_on = binary_comparison(
                expr.as_ref().clone(),
                BinaryOperator::Eq,
                qualified_column("in_subquery", &join_key_name),
            );

            let mut subquery_body = subquery.clone();
            if let SetExpr::Select(sub_select) = subquery_body.body.as_mut() {
                let (left, threshold, op) = extract_policy_comparison(&constraint)?;
                let metric_expr = if let Ok(Expr::Function(function)) = parse_expr(&left) {
                    if let Some(arg) = first_function_expr(&function) {
                        function_call("max", vec![arg.clone()])
                    } else {
                        parse_expr("max(l_quantity)")?
                    }
                } else {
                    parse_expr(&left)?
                };
                sub_select.projection.push(SelectItem::ExprWithAlias {
                    expr: metric_expr,
                    alias: Ident::new("dfc2"),
                });
                extra_dfc.push(ExtraDfcFilter {
                    alias: "dfc2".to_string(),
                    subquery_alias: "in_subquery".to_string(),
                    subquery_metric: "dfc2".to_string(),
                    threshold,
                    op,
                });
            }

            if select.from.is_empty() {
                return Err(RewriteError::unsupported_statement(
                    "IN rewrite requires a FROM clause",
                ));
            }
            select.from[0].joins.push(Join {
                relation: TableFactor::Derived {
                    lateral: false,
                    subquery: subquery_body,
                    alias: Some(TableAlias {
                        name: Ident::new("in_subquery"),
                        columns: Vec::new(),
                    }),
                },
                global: false,
                join_operator: JoinOperator::Inner(JoinConstraint::On(join_on)),
            });
            handled.insert(policy_index);
        }

        select.selection = rebuild_and(remaining);
        Ok((handled, extra_dfc))
    }

    pub(crate) fn rewrite_exists_subqueries_as_joins_impl(
        &self,
        select: &mut Select,
    ) -> Result<HashSet<usize>, RewriteError> {
        let mut handled = HashSet::new();
        let Some(where_expr) = select.selection.take() else {
            return Ok(handled);
        };
        let conjuncts = flatten_and(&where_expr);
        let mut remaining = Vec::new();
        let main_tables = TableScope::from_select(select).direct_base_tables;

        for conjunct in conjuncts {
            let Expr::Exists { subquery, negated } = &conjunct else {
                remaining.push(conjunct);
                continue;
            };
            if *negated {
                remaining.push(conjunct);
                continue;
            }

            let SetExpr::Select(exists_select) = subquery.body.as_ref() else {
                remaining.push(conjunct);
                continue;
            };
            let subquery_tables = select_direct_base_tables(exists_select);
            let Some((policy_index, policy_table)) =
                self.policies
                    .iter()
                    .enumerate()
                    .find_map(|(index, policy)| {
                        let PolicyIr::CompatDfc {
                            sources,
                            on_fail: Resolution::Remove,
                            ..
                        } = policy
                        else {
                            return None;
                        };
                        let policy_source = sources.iter().find(|source| {
                            let key = TableKey::new(source);
                            subquery_tables.contains(&key) && !main_tables.contains(&key)
                        })?;
                        Some((index, TableKey::new(policy_source).as_str().to_string()))
                    })
            else {
                remaining.push(conjunct);
                continue;
            };

            let Some(subquery_where) = exists_select.selection.as_ref() else {
                remaining.push(conjunct);
                continue;
            };
            let Some((outer_col, subquery_col)) =
                extract_exists_join_columns(subquery_where, &policy_table)
            else {
                remaining.push(conjunct);
                continue;
            };

            let PolicyIr::CompatDfc { constraint, .. } = &self.policies[policy_index] else {
                remaining.push(conjunct);
                continue;
            };
            let (agg_projection, agg_alias) =
                exists_subquery_aggregate_projection(constraint, &policy_table)?;
            let other_where =
                remove_join_equality_from_where(subquery_where, &subquery_col, &outer_col);
            let join_subquery_sql = build_exists_join_subquery_sql(
                &policy_table,
                &subquery_col,
                other_where.as_ref(),
                &agg_projection,
                &agg_alias,
            )?;
            let join_statement = parse_query(&join_subquery_sql)?;
            let Statement::Query(join_subquery) = join_statement else {
                return Err(RewriteError::unsupported_statement(
                    "failed to build EXISTS join subquery",
                ));
            };
            let join_key_name = column_name_from_expr(&parse_expr(&subquery_col)?)
                .map(|name| name.as_str().to_string())
                .unwrap_or_else(|| subquery_col.clone());
            let join_on = binary_comparison(
                parse_expr(&outer_col)?,
                BinaryOperator::Eq,
                qualified_column("exists_subquery", &join_key_name),
            );

            if select.from.is_empty() {
                return Err(RewriteError::unsupported_statement(
                    "EXISTS rewrite requires a FROM clause",
                ));
            }
            select.from[0].joins.push(Join {
                relation: TableFactor::Derived {
                    lateral: false,
                    subquery: join_subquery,
                    alias: Some(TableAlias {
                        name: Ident::new("exists_subquery"),
                        columns: Vec::new(),
                    }),
                },
                global: false,
                join_operator: JoinOperator::Inner(JoinConstraint::On(join_on)),
            });

            let having_expr = exists_subquery_having_expr(constraint, &agg_alias)?;
            add_filter(select, having_expr, true)?;
            handled.insert(policy_index);
        }

        select.selection = rebuild_and(remaining);
        Ok(handled)
    }
}
