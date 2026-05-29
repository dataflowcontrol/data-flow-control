//! Tuple- and relation-level resolution rewrites (CTE `t1`–`t4` pattern).

use sqlparser::ast::{
    Expr, Ident, Query, Select, SelectItem, SetExpr, SetOperator, SetQuantifier, TableFactor,
    TableWithJoins, WildcardAdditionalOptions,
};

use crate::diagnostics::RewriteError;
use crate::rewriter::expr::projected_column_name;
use crate::rewriter::projection::ensure_projection_aliases;
use crate::sql::{
    alias_expr, bool_literal, cte, empty_select, function_call, grouped_select, is_not_null,
    passant_kill_pass_filter, qualified_column, query_from_select, table_factor, with_ctes,
};

pub(crate) const T1_CTE: &str = "t1";
pub(crate) const T2_CTE: &str = "t2";
pub(crate) const T3_CTE: &str = "t3";
pub(crate) const T4_CTE: &str = "t4";
pub(crate) const PASS_COLUMN: &str = "__passant_policy_pass";
pub(crate) const RELATION_VIOLATION_COLUMN: &str = "__passant_relation_violation";
pub(crate) const RELATION_INPUT_CTE: &str = "__passant_relation_input";
pub(crate) const PASSANT_KILL_UDF: &str = "passant_kill";

pub(crate) fn wrap_select_with_tuple_resolution(
    mut inner: Select,
    pass_expr: Expr,
    udf_name: &str,
) -> Result<Select, RewriteError> {
    ensure_projection_aliases(&mut inner);
    let output_columns = output_column_names(&inner);
    if output_columns.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "tuple UDF resolution requires a non-empty SELECT projection",
        ));
    }

    let mut t1 = inner;
    t1.projection.push(alias_expr(pass_expr, PASS_COLUMN));
    let t1_query = query_from_select(t1);

    let t2_projection: Vec<SelectItem> = output_columns
        .iter()
        .map(|name| SelectItem::UnnamedExpr(qualified_column(T1_CTE, name)))
        .collect();

    let is_kill = udf_name == PASSANT_KILL_UDF;
    let udf_args: Vec<Expr> = if is_kill {
        Vec::new()
    } else {
        output_columns
            .iter()
            .map(|name| qualified_column(T1_CTE, name))
            .collect()
    };
    let udf_call = function_call(udf_name, udf_args);

    if is_kill {
        let kill_filter = passant_kill_pass_filter(qualified_column(T1_CTE, PASS_COLUMN));
        let final_select = grouped_select(
            t2_projection,
            vec![TableWithJoins {
                relation: table_factor(T1_CTE),
                joins: Vec::new(),
            }],
            Some(kill_filter),
            Vec::new(),
        );
        let final_query = with_ctes(
            vec![cte(T1_CTE, t1_query)],
            SetExpr::Select(Box::new(final_select)),
        );
        let mut outer = empty_select();
        outer.projection = vec![SelectItem::Wildcard(WildcardAdditionalOptions::default())];
        outer.from = vec![TableWithJoins {
            relation: TableFactor::Derived {
                lateral: false,
                subquery: Box::new(final_query),
                alias: Some(crate::sql::table_alias("__passant_tuple_resolution")),
            },
            joins: Vec::new(),
        }];
        return Ok(outer);
    }

    let mut t2 = grouped_select(
        t2_projection.clone(),
        vec![TableWithJoins {
            relation: table_factor(T1_CTE),
            joins: Vec::new(),
        }],
        Some(qualified_column(T1_CTE, PASS_COLUMN)),
        Vec::new(),
    );
    t2.having = None;

    let (t3_projection, t4_projection, t4_filter): (Vec<SelectItem>, Vec<SelectItem>, Expr) =
        if output_columns.len() == 1 {
            let col = &output_columns[0];
            (
                vec![alias_expr(udf_call.clone(), col)],
                vec![SelectItem::UnnamedExpr(qualified_column(T3_CTE, col))],
                is_not_null(qualified_column(T3_CTE, col)),
            )
        } else {
            let resolved = "__passant_resolved_row";
            let t4_cols: Vec<SelectItem> = output_columns
                .iter()
                .map(|name| {
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(vec![
                        Ident::new(resolved),
                        Ident::new(name),
                    ]))
                })
                .collect();
            (
                vec![alias_expr(udf_call.clone(), resolved)],
                t4_cols,
                is_not_null(Expr::Identifier(Ident::new(resolved))),
            )
        };

    let t3 = grouped_select(
        t3_projection,
        vec![TableWithJoins {
            relation: table_factor(T1_CTE),
            joins: Vec::new(),
        }],
        Some(Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Not,
            expr: Box::new(qualified_column(T1_CTE, PASS_COLUMN)),
        }),
        Vec::new(),
    );

    let t4 = if t4_projection.is_empty() {
        Some(grouped_select(
            vec![SelectItem::UnnamedExpr(bool_literal(false))],
            vec![TableWithJoins {
                relation: table_factor(T3_CTE),
                joins: Vec::new(),
            }],
            Some(bool_literal(false)),
            Vec::new(),
        ))
    } else {
        Some(grouped_select(
            t4_projection,
            vec![TableWithJoins {
                relation: table_factor(T3_CTE),
                joins: Vec::new(),
            }],
            Some(t4_filter),
            Vec::new(),
        ))
    };

    let final_body = {
        let t4_select = t4
            .as_ref()
            .expect("t4 required for tuple UDF resolution")
            .clone();
        SetExpr::SetOperation {
            op: SetOperator::Union,
            set_quantifier: SetQuantifier::All,
            left: Box::new(SetExpr::Select(Box::new(t2.clone()))),
            right: Box::new(SetExpr::Select(Box::new(t4_select))),
        }
    };

    let mut ctes = vec![cte(T1_CTE, t1_query), cte(T2_CTE, query_from_select(t2))];
    ctes.push(cte(T3_CTE, query_from_select(t3)));
    if let Some(t4_select) = t4 {
        ctes.push(cte(T4_CTE, query_from_select(t4_select)));
    }

    let final_query = with_ctes(ctes, final_body);

    let mut outer = empty_select();
    outer.projection = vec![SelectItem::Wildcard(WildcardAdditionalOptions::default())];
    outer.from = vec![TableWithJoins {
        relation: TableFactor::Derived {
            lateral: false,
            subquery: Box::new(final_query),
            alias: Some(crate::sql::table_alias("__passant_tuple_resolution")),
        },
        joins: Vec::new(),
    }];
    Ok(outer)
}

pub(crate) fn wrap_query_with_relation_resolution(
    inner: Query,
    violation_expr: Expr,
    udf_name: &str,
) -> Result<Query, RewriteError> {
    let SetExpr::Select(select) = inner.body.as_ref() else {
        return Err(RewriteError::unsupported_statement(
            "relation UDF resolution requires a SELECT query body",
        ));
    };
    let output_columns = output_column_names(select);
    if output_columns.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "relation UDF resolution requires a non-empty SELECT projection",
        ));
    }

    let mut annotated = select.as_ref().clone();
    ensure_projection_aliases(&mut annotated);
    annotated
        .projection
        .push(alias_expr(violation_expr, RELATION_VIOLATION_COLUMN));
    let annotated_query = query_from_select(annotated);

    let bool_or_subquery = Expr::Subquery(Box::new(Query {
        with: None,
        body: Box::new(SetExpr::Select(Box::new(grouped_select(
            vec![SelectItem::UnnamedExpr(function_call(
                "bool_or",
                vec![qualified_column(
                    RELATION_INPUT_CTE,
                    RELATION_VIOLATION_COLUMN,
                )],
            ))],
            vec![TableWithJoins {
                relation: table_factor(RELATION_INPUT_CTE),
                joins: Vec::new(),
            }],
            None,
            Vec::new(),
        )))),
        order_by: None,
        limit: None,
        limit_by: Vec::new(),
        offset: None,
        fetch: None,
        locks: Vec::new(),
        for_clause: None,
        settings: None,
        format_clause: None,
    }));

    let mut resolved = empty_select();
    resolved.projection = output_columns
        .iter()
        .map(|name| SelectItem::UnnamedExpr(qualified_column(RELATION_INPUT_CTE, name)))
        .collect();
    resolved.from = vec![TableWithJoins {
        relation: table_factor(RELATION_INPUT_CTE),
        joins: Vec::new(),
    }];
    resolved.selection = Some(function_call(udf_name, vec![bool_or_subquery]));

    Ok(with_ctes(
        vec![cte(RELATION_INPUT_CTE, annotated_query)],
        SetExpr::Select(Box::new(resolved)),
    ))
}

pub(crate) fn combine_violation_exprs(exprs: Vec<Expr>) -> Expr {
    let Some(first) = exprs.into_iter().reduce(|left, right| Expr::BinaryOp {
        left: Box::new(left),
        op: sqlparser::ast::BinaryOperator::Or,
        right: Box::new(Expr::Nested(Box::new(right))),
    }) else {
        return bool_literal(false);
    };
    Expr::UnaryOp {
        op: sqlparser::ast::UnaryOperator::Not,
        expr: Box::new(first),
    }
}

fn output_column_names(select: &Select) -> Vec<String> {
    select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::UnnamedExpr(expr) => projected_column_name(expr),
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{binary_comparison, render_statement};

    #[test]
    fn tuple_resolution_emits_t1_through_t4_ctes() {
        let mut inner = empty_select();
        inner.projection = vec![alias_expr(qualified_column("foo", "id"), "id")];
        inner.from = vec![TableWithJoins {
            relation: table_factor("foo"),
            joins: Vec::new(),
        }];
        let wrapped = wrap_select_with_tuple_resolution(
            inner,
            binary_comparison(
                qualified_column("foo", "id"),
                sqlparser::ast::BinaryOperator::Gt,
                crate::sql::int_literal(0),
            ),
            "repair_row",
        )
        .expect("wrap");
        let sql = render_statement(
            &crate::sql::statement_from_query(query_from_select(wrapped)),
            None,
        );
        assert!(sql.contains("t1 AS"));
        assert!(sql.contains("t2 AS"));
        assert!(sql.contains("t3 AS"));
        assert!(sql.contains("t4 AS"));
        assert!(sql.contains("UNION ALL"));
        assert!(sql.contains("repair_row"));
        assert!(sql.contains(PASS_COLUMN));
    }
}
