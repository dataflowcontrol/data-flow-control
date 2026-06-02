use std::collections::HashSet;

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, Ident, Join, JoinConstraint, JoinOperator, Select,
    SelectItem, SetExpr, Statement, TableAlias, TableFactor, TableWithJoins,
};

use crate::identifiers::{TableKey, column_name_from_expr, table_name_from_column_expr};
use crate::parser::parse_query;
use crate::partial_push::ExtraDfcFilter;
use crate::policy::{PolicyIr, Resolution};
use crate::policy_store::MultiSourceLookupMode;
use crate::sql::{
    alias_expr, binary_comparison, column_comparison, expr_keys_equal, function_call,
    grouped_select, passant_filter_temp_column, qualified_column, query_from_select,
    statement_from_query, unqualify_table_refs,
};

use super::RewriteError;
use super::expr::{
    add_filter, first_function_expr, is_aggregate_name, parse_expr, projection_expr_and_name,
    table_factor_base_and_alias,
};
use super::helpers::{flatten_and, rebuild_and};
use super::projection::extract_policy_comparison_for_policy;
use super::scope::TableScope;
use super::types::PassantRewriter;

fn select_direct_base_tables(select: &Select) -> HashSet<TableKey> {
    TableScope::from_select(select).direct_base_tables
}

fn table_with_joins_contains_base(table: &TableWithJoins, base: &str) -> bool {
    let target = TableKey::new(base);
    if table_factor_base_and_alias(&table.relation)
        .is_some_and(|(name, _)| TableKey::new(&name) == target)
    {
        return true;
    }
    table.joins.iter().any(|join| {
        table_factor_base_and_alias(&join.relation)
            .is_some_and(|(name, _)| TableKey::new(&name) == target)
    })
}

fn from_index_for_in_outer_expr(select: &Select, outer_expr: &Expr) -> usize {
    if let Some(table_name) = table_name_from_column_expr(outer_expr) {
        let target = table_name.as_str();
        for (index, table_with_joins) in select.from.iter().enumerate() {
            if table_with_joins_contains_base(table_with_joins, target) {
                return index;
            }
        }
    }
    select.from.len().saturating_sub(1)
}

fn qualify_unqualified_column_in_expr(expr: &mut Expr, column: &str, table: &str) {
    match expr {
        Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case(column) => {
            *expr = qualified_column(table, column);
        }
        Expr::BinaryOp { left, right, .. } => {
            qualify_unqualified_column_in_expr(left, column, table);
            qualify_unqualified_column_in_expr(right, column, table);
        }
        Expr::Nested(inner) => qualify_unqualified_column_in_expr(inner, column, table),
        _ => {}
    }
}

fn qualify_join_key_in_remaining(remaining: &mut [Expr], join_key_name: &str, qualify_table: &str) {
    for expr in remaining.iter_mut() {
        qualify_unqualified_column_in_expr(expr, join_key_name, qualify_table);
    }
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
            return Some((
                crate::sql::render_expr(right.as_ref(), None),
                crate::sql::render_expr(left.as_ref(), None),
            ));
        }
        if right_is_subquery && !left_is_subquery {
            return Some((
                crate::sql::render_expr(left.as_ref(), None),
                crate::sql::render_expr(right.as_ref(), None),
            ));
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
                if (crate::sql::expr_key_matches_str(left, subquery_col)
                    && crate::sql::expr_key_matches_str(right, outer_col))
                    || (crate::sql::expr_key_matches_str(left, outer_col)
                        && crate::sql::expr_key_matches_str(right, subquery_col)))
        })
        .collect::<Vec<_>>();
    rebuild_and(remaining)
}

fn exists_subquery_aggregate_projection(
    constraint: &Expr,
    policy_table: &str,
    registry: &crate::aggregate_registry::AggregateRegistry,
) -> Result<(SelectItem, String), RewriteError> {
    let Expr::BinaryOp { left, .. } = constraint else {
        return Err(RewriteError::unsupported_statement(
            "EXISTS policy constraint must be a comparison",
        ));
    };
    let Expr::Function(function) = left.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "EXISTS policy constraint must use an aggregate",
        ));
    };
    if !is_aggregate_name(&function.name.to_string(), registry) {
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
        .unwrap_or_else(|| crate::sql::render_expr(&join_key_expr, None));
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
    Ok(crate::sql::render_statement(
        &statement_from_query(query_from_select(select)),
        None,
    ))
}

fn exists_subquery_having_expr(
    constraint: &Expr,
    agg_alias: &str,
    registry: &crate::aggregate_registry::AggregateRegistry,
) -> Result<Expr, RewriteError> {
    let Expr::BinaryOp {
        mut left,
        op,
        right,
    } = constraint.clone()
    else {
        return Err(RewriteError::unsupported_statement(
            "EXISTS policy constraint must be a comparison",
        ));
    };
    replace_aggregate_with_exists_subquery(&mut left, agg_alias, registry);
    Ok(Expr::BinaryOp { left, op, right })
}

fn replace_aggregate_with_exists_subquery(
    expr: &mut Expr,
    agg_alias: &str,
    registry: &crate::aggregate_registry::AggregateRegistry,
) {
    match expr {
        Expr::Function(function) if registry.is_aggregate_call(function) => {
            *expr = function_call("max", vec![qualified_column("exists_subquery", agg_alias)]);
        }
        Expr::BinaryOp { left, right, .. } => {
            replace_aggregate_with_exists_subquery(left, agg_alias, registry);
            replace_aggregate_with_exists_subquery(right, agg_alias, registry);
        }
        Expr::Nested(inner) => replace_aggregate_with_exists_subquery(inner, agg_alias, registry),
        _ => {}
    }
}

fn subquery_groups_by_join_key(select: &Select, join_key_expr: &Expr) -> bool {
    let GroupByExpr::Expressions(group_exprs, _) = &select.group_by else {
        return false;
    };
    group_exprs
        .iter()
        .any(|group_expr| expr_keys_equal(group_expr, join_key_expr))
}

fn ensure_in_subquery_metric_grouped(
    sub_select: &mut Select,
    join_key_expr: &Expr,
    join_key_item: SelectItem,
    metric_expr: Expr,
    metric_alias: &str,
) {
    if subquery_groups_by_join_key(sub_select, join_key_expr) {
        sub_select.projection.push(SelectItem::ExprWithAlias {
            expr: metric_expr,
            alias: Ident::new(metric_alias),
        });
        return;
    }

    sub_select.distinct = None;
    sub_select.projection = vec![
        join_key_item,
        SelectItem::ExprWithAlias {
            expr: metric_expr,
            alias: Ident::new(metric_alias),
        },
    ];
    sub_select.group_by = GroupByExpr::Expressions(vec![join_key_expr.clone()], Vec::new());
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
        let mut join_key_qualifications: Vec<(String, String)> = Vec::new();

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
            let mut candidate_tables = main_tables.clone();
            candidate_tables.extend(subquery_tables.iter().cloned());
            let Some((policy_index, constraint)) = self
                .store
                .candidate_scope_lookup(&candidate_tables, None, MultiSourceLookupMode::Subset)
                .iter()
                .find_map(|index| {
                    let policy = self.store.policy(index)?;
                    let PolicyIr::Pgn {
                        sources,
                        constraint,
                        on_fail: Resolution::Remove,
                        ..
                    } = policy
                    else {
                        return None;
                    };
                    if !sources
                        .iter()
                        .any(|source| subquery_tables.contains(&TableKey::new(source)))
                    {
                        return None;
                    }
                    Some((index, constraint.to_string()))
                })
            else {
                remaining.push(conjunct);
                continue;
            };

            let subquery_only_policy = self.store.policy(policy_index).is_some_and(|policy| {
                let PolicyIr::Pgn { sources, .. } = policy;
                sources.iter().all(|source| {
                    let key = TableKey::new(source);
                    subquery_tables.contains(&key) && !main_tables.contains(&key)
                })
            });

            let constraint_ctx = crate::rewriter::policy_expr::ConstraintExprCtx {
                store: &self.store,
                index: policy_index,
                stats: None,
            };

            let Some((join_key_expr, _)) = in_select.projection.first().and_then(|item| {
                projection_expr_and_name(item).map(|(expr, alias)| (expr.clone(), alias))
            }) else {
                remaining.push(conjunct);
                continue;
            };
            let join_key_col = crate::sql::render_expr(&join_key_expr, None);
            let join_key_name = column_name_from_expr(&join_key_expr)
                .map(|name| name.as_str().to_string())
                .unwrap_or_else(|| join_key_col.clone());
            let join_key_item =
                in_select.projection.first().cloned().expect(
                    "join key projection item exists when join key expression was extracted",
                );
            let join_on = binary_comparison(
                expr.as_ref().clone(),
                BinaryOperator::Eq,
                qualified_column("in_subquery", &join_key_name),
            );

            let mut subquery_body = subquery.clone();
            if let SetExpr::Select(sub_select) = subquery_body.body.as_mut() {
                let (left, threshold, op) = extract_policy_comparison_for_policy(
                    &self.store,
                    policy_index,
                    &constraint,
                    None,
                )?;
                let constraint_expr = constraint_ctx.expr(&constraint)?;
                let metric_expr = if let Expr::BinaryOp { left, .. } = &constraint_expr {
                    if let Expr::Function(function) = left.as_ref() {
                        if let Some(arg) = first_function_expr(function) {
                            function_call("max", vec![arg.clone()])
                        } else {
                            parse_expr("max(l_quantity)")?
                        }
                    } else {
                        left.as_ref().clone()
                    }
                } else {
                    parse_expr(&left)?
                };
                let subquery_metric_alias =
                    passant_filter_temp_column(&format!("in_metric_{policy_index}"));
                let outer_metric_alias =
                    passant_filter_temp_column(&format!("in_agg_{policy_index}"));
                ensure_in_subquery_metric_grouped(
                    sub_select,
                    &join_key_expr,
                    join_key_item,
                    metric_expr,
                    &subquery_metric_alias,
                );
                extra_dfc.push(ExtraDfcFilter {
                    alias: outer_metric_alias,
                    subquery_alias: "in_subquery".to_string(),
                    subquery_metric: subquery_metric_alias,
                    threshold,
                    op,
                });
            }

            if select.from.is_empty() {
                return Err(RewriteError::unsupported_statement(
                    "IN rewrite requires a FROM clause",
                ));
            }
            let from_index = from_index_for_in_outer_expr(select, expr.as_ref());
            select.from[from_index].joins.push(Join {
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
            if subquery_only_policy {
                handled.insert(policy_index);
            }
            if let Some(PolicyIr::Pgn { sources, .. }) = self.store.policy(policy_index)
                && let Some(source) = sources
                    .iter()
                    .find(|source| main_tables.contains(&TableKey::new(source)))
            {
                join_key_qualifications.push((join_key_name.clone(), source.clone()));
            }
        }

        for (join_key_name, qualify_table) in join_key_qualifications {
            qualify_join_key_in_remaining(&mut remaining, &join_key_name, &qualify_table);
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
            let mut candidate_tables = main_tables.clone();
            candidate_tables.extend(subquery_tables.iter().cloned());
            let Some((policy_index, policy_table)) = self
                .store
                .candidate_scope_lookup(&candidate_tables, None, MultiSourceLookupMode::Subset)
                .iter()
                .find_map(|index| {
                    let policy = self.store.policy(index)?;
                    let PolicyIr::Pgn {
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

            let Some(PolicyIr::Pgn { constraint, .. }) = self.store.policy(policy_index) else {
                remaining.push(conjunct);
                continue;
            };
            let constraint_expr = self.store.constraint_expr(policy_index, constraint, None)?;
            let (agg_projection, agg_alias) = exists_subquery_aggregate_projection(
                &constraint_expr,
                &policy_table,
                &self.aggregate_registry,
            )?;
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

            let having_expr = exists_subquery_having_expr(
                &constraint_expr,
                &agg_alias,
                &self.aggregate_registry,
            )?;
            add_filter(select, having_expr, true)?;
            handled.insert(policy_index);
        }

        select.selection = rebuild_and(remaining);
        Ok(handled)
    }
}

pub(crate) fn apply_in_semijoin_policy_filters(
    select: &mut Select,
    extra_dfc: &[ExtraDfcFilter],
    is_aggregation: bool,
    context: &mut super::types::RewriteContext,
) -> Result<(), RewriteError> {
    for extra in extra_dfc {
        let metric = qualified_column(&extra.subquery_alias, &extra.subquery_metric);

        if context.defer_policy_for_outer_limit && is_aggregation {
            let alias = passant_filter_temp_column(&format!(
                "filter_agg_{}",
                context.pending_in_semijoin_filters.len()
            ));
            select.projection.push(SelectItem::ExprWithAlias {
                expr: function_call("max", vec![metric]),
                alias: Ident::new(&alias),
            });
            context.pending_in_semijoin_filters.push(ExtraDfcFilter {
                alias,
                subquery_alias: extra.subquery_alias.clone(),
                subquery_metric: extra.subquery_metric.clone(),
                threshold: extra.threshold.clone(),
                op: extra.op,
            });
            continue;
        }

        if is_aggregation {
            let filter = binary_comparison(
                function_call("max", vec![metric]),
                semijoin_op(extra.op),
                parse_expr(&extra.threshold)?,
            );
            add_filter(select, filter, true)?;
        } else {
            let filter = column_comparison(
                &crate::sql::render_expr(&metric, None),
                extra.op,
                parse_expr(&extra.threshold)?,
            )
            .ok_or_else(|| {
                RewriteError::unsupported_statement(
                    "IN semijoin policy rewrite uses unsupported comparison operator",
                )
            })?;
            add_filter(select, filter, false)?;
        }
    }
    Ok(())
}

fn semijoin_op(op: &str) -> BinaryOperator {
    match op {
        ">" => BinaryOperator::Gt,
        ">=" => BinaryOperator::GtEq,
        "<" => BinaryOperator::Lt,
        "<=" => BinaryOperator::LtEq,
        "=" => BinaryOperator::Eq,
        "!=" | "<>" => BinaryOperator::NotEq,
        _ => BinaryOperator::GtEq,
    }
}
