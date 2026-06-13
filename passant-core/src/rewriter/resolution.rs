//! Tuple- and relation-level resolution rewrites (CTE `t1`–`t4` pattern).

use std::collections::{HashMap, HashSet};

use sqlparser::ast::{
    Assignment, Expr, Ident, Join, JoinOperator, Query, Select, SelectItem, SetExpr, SetOperator,
    SetQuantifier, TableFactor, TableWithJoins, WildcardAdditionalOptions,
};

use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::identifiers::{ColumnKey, TableKey};
use crate::policy::PolicyIr;
use crate::policy_store::PolicyStore;
use crate::rewriter::expr::projected_column_name;
use crate::rewriter::helpers::select_output_column_mapping;
use crate::rewriter::projection::ensure_projection_aliases;
use crate::rewriter::scope::TableScope;
use crate::rewriter::types::RewriteContext;
use crate::sql::{
    alias_expr, bool_literal, case_when, cte, empty_select, function_call, grouped_select,
    is_not_null, passant_kill_pass_filter, qualified_column, query_from_select, string_literal,
    table_factor, with_ctes,
};

pub(crate) const T1_CTE: &str = "t1";
pub(crate) const T2_CTE: &str = "t2";
pub(crate) const T3_CTE: &str = "t3";
pub(crate) const T4_CTE: &str = "t4";
pub(crate) const PASS_COLUMN: &str = "__passant_policy_pass";
pub(crate) const RELATION_VIOLATION_COLUMN: &str = "__passant_relation_violation";
pub(crate) const RELATION_INPUT_CTE: &str = "__passant_relation_input";
pub(crate) const RELATION_AGG_CTE: &str = "__passant_relation_agg";
pub(crate) const RELATION_BATCH_VIOLATION_COLUMN: &str = "__passant_batch_violation";
pub(crate) const PASSANT_KILL_UDF: &str = "passant_kill";
pub(crate) const PASSANT_KILL_WRAPPER: &str = "__passant_kill";
pub(crate) const UI_ADDRESS_VIOLATING_ROWS_UDF: &str = "address_violating_rows";
pub(crate) const PASSANT_UI_APPROVE_UDF: &str = "passant_ui_approve";

/// After KILL wrapping, the outer query is `SELECT * FROM __passant_kill` while
/// `ORDER BY` may still reference the original table aliases. Unqualify them.
pub(crate) fn remap_query_order_by_after_kill_wrap(query: &mut Query) {
    if query.order_by.is_none() {
        return;
    }
    let SetExpr::Select(select) = query.body.as_mut() else {
        return;
    };
    if !is_kill_wrapped_select(select) {
        return;
    }
    if let Some(order_by) = query.order_by.as_mut() {
        for item in &mut order_by.exprs {
            crate::rewriter::columns::unqualify_columns(&mut item.expr);
        }
    }
}

fn is_kill_wrapped_select(select: &Select) -> bool {
    if select.projection.len() != 1 {
        return false;
    }
    if !matches!(select.projection.first(), Some(SelectItem::Wildcard(_))) {
        return false;
    }
    let Some(TableWithJoins {
        relation: TableFactor::Derived {
            alias: Some(alias), ..
        },
        joins,
        ..
    }) = select.from.first()
    else {
        return false;
    };
    alias.name.value == PASSANT_KILL_WRAPPER && joins.is_empty()
}

#[derive(Debug, Clone)]
pub(crate) struct UiResolutionSpec {
    pub constraint: String,
    pub description: Option<String>,
    pub sink: Option<String>,
    pub policy_index: usize,
}

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
            t2_projection.clone(),
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
                alias: Some(crate::sql::table_alias(PASSANT_KILL_WRAPPER)),
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

    let agg_select = grouped_select(
        vec![alias_expr(
            function_call(
                "bool_or",
                vec![qualified_column(
                    RELATION_INPUT_CTE,
                    RELATION_VIOLATION_COLUMN,
                )],
            ),
            RELATION_BATCH_VIOLATION_COLUMN,
        )],
        vec![TableWithJoins {
            relation: table_factor(RELATION_INPUT_CTE),
            joins: Vec::new(),
        }],
        None,
        Vec::new(),
    );

    let mut resolved = empty_select();
    resolved.projection = output_columns
        .iter()
        .map(|name| SelectItem::UnnamedExpr(qualified_column(RELATION_INPUT_CTE, name)))
        .collect();
    resolved.from = vec![TableWithJoins {
        relation: table_factor(RELATION_INPUT_CTE),
        joins: vec![Join {
            relation: table_factor(RELATION_AGG_CTE),
            global: false,
            join_operator: JoinOperator::CrossJoin,
        }],
    }];
    resolved.selection = Some(function_call(
        udf_name,
        vec![qualified_column(
            RELATION_AGG_CTE,
            RELATION_BATCH_VIOLATION_COLUMN,
        )],
    ));

    Ok(with_ctes(
        vec![
            cte(RELATION_INPUT_CTE, annotated_query),
            cte(RELATION_AGG_CTE, query_from_select(agg_select)),
        ],
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn ui_pass_filter(
    pass_expr: Expr,
    select: &mut Select,
    spec: &UiResolutionSpec,
    store: &PolicyStore,
    policy: &PolicyIr,
    context: &RewriteContext,
    table_scope: &TableScope,
    catalog: &TableCatalog,
) -> Result<Expr, RewriteError> {
    ensure_projection_aliases(select);
    let (column_names, column_exprs) = collect_ui_udf_columns(
        &*select,
        store,
        spec.policy_index,
        policy,
        context,
        table_scope,
        catalog,
    )?;
    let column_names_json = serde_json::to_string(&column_names).map_err(|err| {
        RewriteError::unsupported_statement(format!(
            "UI resolution could not serialize column names: {err}"
        ))
    })?;
    let description = spec.description.clone().unwrap_or_default();
    let stream_endpoint = context.ui_stream_endpoint.clone().unwrap_or_default();

    let mut udf_args = column_exprs;
    udf_args.push(string_literal(&spec.constraint));
    udf_args.push(string_literal(&description));
    udf_args.push(string_literal(&column_names_json));
    udf_args.push(string_literal(&stream_endpoint));

    let udf_call = function_call(UI_ADDRESS_VIOLATING_ROWS_UDF, udf_args);
    Ok(case_when(pass_expr, bool_literal(true), udf_call))
}

pub(crate) fn ui_approval_pass_filter_from_columns(
    pass_expr: Expr,
    column_names: &[String],
    column_exprs: Vec<Expr>,
    constraint: &str,
    description: Option<&str>,
) -> Result<Expr, RewriteError> {
    let column_names_json = serde_json::to_string(column_names).map_err(|err| {
        RewriteError::unsupported_statement(format!(
            "UI resolution could not serialize column names: {err}"
        ))
    })?;
    let description = description.unwrap_or("").to_string();
    let mut udf_args = column_exprs;
    udf_args.push(string_literal(constraint));
    udf_args.push(string_literal(&description));
    udf_args.push(string_literal(&column_names_json));
    let udf_call = function_call(PASSANT_UI_APPROVE_UDF, udf_args);
    Ok(case_when(pass_expr, bool_literal(true), udf_call))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_ui_update_udf_columns(
    assignments: &[Assignment],
    target_table: &str,
    store: &PolicyStore,
    policy_index: usize,
    policy: &PolicyIr,
    table_scope: &TableScope,
    catalog: &TableCatalog,
    identity_columns: &[String],
) -> Result<(Vec<String>, Vec<Expr>), RewriteError> {
    let mut select = empty_select();
    for assignment in assignments {
        let col = match &assignment.target {
            sqlparser::ast::AssignmentTarget::ColumnName(name) => {
                name.0.last().map(|i| i.value.clone())
            }
            _ => None,
        };
        if let Some(col) = col {
            select
                .projection
                .push(alias_expr(qualified_column(target_table, &col), &col));
        }
    }
    for col in identity_columns {
        if !select
            .projection
            .iter()
            .any(|item| projected_column_name_from_item(item).as_deref() == Some(col.as_str()))
        {
            select
                .projection
                .push(alias_expr(qualified_column(target_table, col), col));
        }
    }
    let context = RewriteContext::default();
    collect_ui_udf_columns(
        &select,
        store,
        policy_index,
        policy,
        &context,
        table_scope,
        catalog,
    )
}

fn projected_column_name_from_item(item: &SelectItem) -> Option<String> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
        SelectItem::UnnamedExpr(expr) => projected_column_name(expr),
        _ => None,
    }
}

/// Stream/read_csv column order for edited UPDATE: identity columns first, then SET targets.
pub(crate) fn ui_edited_update_stream_column_names(
    assignments: &[Assignment],
    identity_columns: &[String],
) -> Result<Vec<String>, RewriteError> {
    if identity_columns.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "UI edited UPDATE requires target row identity columns (primary key or register unique columns in catalog)",
        ));
    }
    let mut stream_columns = identity_columns.to_vec();
    for assignment in assignments {
        let col = assignment_column_name(assignment)?;
        if !stream_columns.contains(&col) {
            stream_columns.push(col);
        }
    }
    Ok(stream_columns)
}

fn assignment_column_name(assignment: &Assignment) -> Result<String, RewriteError> {
    match &assignment.target {
        sqlparser::ast::AssignmentTarget::ColumnName(name) => {
            name.0.last().map(|i| i.value.clone()).ok_or_else(|| {
                RewriteError::unsupported_statement(
                    "UI edited UPDATE requires simple column assignment targets",
                )
            })
        }
        _ => Err(RewriteError::unsupported_statement(
            "UI edited UPDATE requires simple column assignment targets",
        )),
    }
}

/// Build `UPDATE target SET ... FROM read_csv(stream) AS staged WHERE target.pk = staged.pk`.
pub(crate) fn ui_edited_update_followup_sql(
    target_table: &str,
    assignments: &[Assignment],
    identity_columns: &[String],
    stream_endpoint: &str,
) -> Result<String, RewriteError> {
    if stream_endpoint.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "UI edited UPDATE requires a configured stream endpoint",
        ));
    }
    let stream_columns = ui_edited_update_stream_column_names(assignments, identity_columns)?;
    let mut set_parts = Vec::new();
    for assignment in assignments {
        let col = assignment_column_name(assignment)?;
        set_parts.push(format!("{col} = staged.{col}", col = col));
    }
    let join_cond = identity_columns
        .iter()
        .map(|col| {
            format!(
                "{target}.{col} = staged.{col}",
                target = target_table,
                col = col
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let endpoint_escaped = stream_endpoint.replace('\'', "''");
    let names_list = stream_columns
        .iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!(
        "UPDATE {target} SET {set_clause} FROM (SELECT * FROM read_csv('{endpoint}', delim='\\t', header=false, names=[{names_list}])) staged WHERE {join_cond}",
        target = target_table,
        set_clause = set_parts.join(", "),
        endpoint = endpoint_escaped,
        names_list = names_list,
    ))
}

fn collect_ui_udf_columns(
    select: &Select,
    store: &PolicyStore,
    policy_index: usize,
    policy: &PolicyIr,
    _context: &RewriteContext,
    table_scope: &TableScope,
    catalog: &TableCatalog,
) -> Result<(Vec<String>, Vec<Expr>), RewriteError> {
    let PolicyIr::Pgn {
        sources,
        source_aliases,
        ..
    } = policy;
    let source_keys: HashSet<TableKey> = sources.iter().map(|s| TableKey::new(s)).collect();

    let compiled = store.compiled(policy_index).ok_or_else(|| {
        RewriteError::unsupported_statement("UI resolution requires a compiled policy")
    })?;

    let mut source_names = Vec::new();
    let mut source_exprs = Vec::new();
    let mut seen_source = HashSet::new();

    for (table_key, column_key) in &compiled.constraint_referenced_columns {
        if !source_keys.contains(table_key) {
            continue;
        }
        let (dotted, expr) =
            resolve_policy_column_expr(table_key, column_key, source_aliases, table_scope)?;
        if seen_source.insert(dotted.clone()) {
            source_names.push(dotted);
            source_exprs.push(expr);
        }
    }

    let output_mapping = select_output_column_mapping(select, catalog)?;
    if output_mapping.expr_by_column.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "UI resolution requires a named SELECT projection (SELECT * without catalog expansion is not supported)",
        ));
    }
    if !output_mapping.ambiguous_columns.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "UI resolution requires unambiguous output column names",
        ));
    }

    let mut column_names = source_names;
    let mut column_exprs = source_exprs;
    let source_name_set: HashSet<String> = column_names.iter().cloned().collect();

    let output_names: Vec<String> = select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::UnnamedExpr(expr) => projected_column_name(expr),
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            _ => None,
        })
        .collect();

    for name in output_names {
        let key = name.to_ascii_lowercase();
        if source_name_set.contains(&name) {
            continue;
        }
        let Some(expr) = output_mapping.expr_by_column.get(&key) else {
            return Err(RewriteError::unsupported_statement(format!(
                "UI resolution could not resolve output column {name}"
            )));
        };
        column_names.push(name);
        column_exprs.push(expr.clone());
    }

    if column_names.is_empty() {
        return Err(RewriteError::unsupported_statement(
            "UI resolution requires at least one source or output column",
        ));
    }

    Ok((column_names, column_exprs))
}

fn resolve_policy_column_expr(
    table_key: &TableKey,
    column_key: &ColumnKey,
    source_aliases: &HashMap<String, String>,
    table_scope: &TableScope,
) -> Result<(String, Expr), RewriteError> {
    let table_base = table_key.as_str();
    let dotted = format!("{table_base}.{}", column_key.as_str());
    let qualifier = source_aliases
        .get(table_base)
        .map(String::as_str)
        .or_else(|| table_scope.alias_by_base.get_by_table_key(table_base))
        .unwrap_or(table_base);
    Ok((dotted, qualified_column(qualifier, column_key.as_str())))
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
    fn relation_resolution_fuses_bool_or_into_agg_cte() {
        let mut inner = empty_select();
        inner.projection = vec![alias_expr(qualified_column("expenses", "amount"), "amount")];
        inner.from = vec![TableWithJoins {
            relation: table_factor("expenses"),
            joins: Vec::new(),
        }];
        let query = query_from_select(inner);
        let wrapped = wrap_query_with_relation_resolution(
            query,
            binary_comparison(
                qualified_column("expenses", "amount"),
                sqlparser::ast::BinaryOperator::Gt,
                crate::sql::int_literal(100),
            ),
            "abort_batch",
        )
        .expect("wrap");
        let sql = render_statement(&crate::sql::statement_from_query(wrapped), None);
        assert!(sql.contains(RELATION_INPUT_CTE));
        assert!(sql.contains(RELATION_AGG_CTE));
        assert!(sql.contains(RELATION_BATCH_VIOLATION_COLUMN));
        assert!(sql.contains("bool_or"));
        assert!(sql.contains("abort_batch"));
        assert!(sql.contains(&format!(
            "{RELATION_AGG_CTE}.{RELATION_BATCH_VIOLATION_COLUMN}"
        )));
    }

    #[test]
    fn kill_resolution_uses_single_cte_without_union_branches() {
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
            PASSANT_KILL_UDF,
        )
        .expect("wrap");
        let sql = render_statement(
            &crate::sql::statement_from_query(query_from_select(wrapped)),
            None,
        );
        assert!(sql.contains("t1 AS"));
        assert!(sql.contains("passant_kill"));
        assert!(!sql.contains("t2 AS"));
        assert!(!sql.contains("t3 AS"));
        assert!(!sql.contains("t4 AS"));
        assert!(!sql.contains("UNION ALL"));
        assert!(!sql.contains("__passant_tuple_resolution"));
        assert!(sql.contains("__passant_kill"));
    }

    #[test]
    fn kill_wrap_remaps_qualified_order_by() {
        let mut inner = empty_select();
        inner.projection = vec![alias_expr(qualified_column("receipts", "id"), "id")];
        inner.from = vec![TableWithJoins {
            relation: table_factor("receipts"),
            joins: Vec::new(),
        }];
        let mut query = query_from_select(inner);
        query.order_by = Some(sqlparser::ast::OrderBy {
            exprs: vec![sqlparser::ast::OrderByExpr {
                expr: qualified_column("receipts", "id"),
                asc: None,
                nulls_first: None,
                with_fill: None,
            }],
            interpolate: None,
        });

        let wrapped = wrap_select_with_tuple_resolution(
            match query.body.as_ref() {
                SetExpr::Select(select) => select.as_ref().clone(),
                _ => unreachable!(),
            },
            binary_comparison(
                qualified_column("receipts", "id"),
                sqlparser::ast::BinaryOperator::Gt,
                crate::sql::int_literal(0),
            ),
            PASSANT_KILL_UDF,
        )
        .expect("wrap");
        query.body = Box::new(SetExpr::Select(Box::new(wrapped)));
        remap_query_order_by_after_kill_wrap(&mut query);

        let sql = render_statement(&crate::sql::statement_from_query(query), None);
        assert!(
            sql.contains("ORDER BY id"),
            "expected unqualified ORDER BY: {sql}"
        );
        assert!(
            !sql.contains("ORDER BY receipts."),
            "qualified ORDER BY should be remapped: {sql}"
        );
    }

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
