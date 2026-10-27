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
    is_not_null, passant_kill_tuple_filter, qualified_column, query_from_select, string_literal,
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
pub(crate) fn remap_query_order_by_after_kill_wrap(
    query: &mut Query,
    context: Option<&RewriteContext>,
) {
    if query.order_by.is_none() {
        return;
    }
    let SetExpr::Select(select) = query.body.as_mut() else {
        return;
    };
    if !is_kill_wrapped_select(select) && !is_joint_resolution_wrapped_select(select) {
        return;
    }
    if let Some(order_by) = query.order_by.as_mut() {
        for item in &mut order_by.exprs {
            crate::rewriter::columns::unqualify_columns(&mut item.expr);
            if let Some(context) = context {
                remap_insert_order_by_expr(&mut item.expr, context);
            }
        }
    }
}

fn remap_insert_order_by_expr(expr: &mut Expr, context: &RewriteContext) {
    if context.sink_expr_by_column.is_empty() {
        return;
    }
    let Expr::Identifier(ident) = expr else {
        return;
    };
    let referenced = ident.value.to_ascii_lowercase();
    let mut matches = Vec::new();
    for (column, output_expr) in &context.sink_expr_by_column {
        if output_expr_references_name(output_expr, &referenced) {
            matches.push(column.clone());
        }
    }
    if matches.len() == 1 {
        ident.value = matches[0].clone();
    }
}

fn output_expr_references_name(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case(name),
        Expr::CompoundIdentifier(parts) if !parts.is_empty() => parts
            .last()
            .is_some_and(|part| part.value.eq_ignore_ascii_case(name)),
        Expr::BinaryOp { left, right, .. } => {
            output_expr_references_name(left, name) || output_expr_references_name(right, name)
        }
        Expr::Nested(inner) => output_expr_references_name(inner, name),
        Expr::Function(function) => function_args_references_name(function, name),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand
                .as_ref()
                .is_some_and(|expr| output_expr_references_name(expr, name))
                || conditions
                    .iter()
                    .any(|expr| output_expr_references_name(expr, name))
                || results
                    .iter()
                    .any(|expr| output_expr_references_name(expr, name))
                || else_result
                    .as_ref()
                    .is_some_and(|expr| output_expr_references_name(expr, name))
        }
        _ => false,
    }
}

fn function_args_references_name(function: &sqlparser::ast::Function, name: &str) -> bool {
    let sqlparser::ast::FunctionArguments::List(args) = &function.args else {
        return false;
    };
    args.args.iter().any(|arg| match arg {
        sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr))
        | sqlparser::ast::FunctionArg::Named {
            arg: sqlparser::ast::FunctionArgExpr::Expr(expr),
            ..
        }
        | sqlparser::ast::FunctionArg::ExprNamed {
            arg: sqlparser::ast::FunctionArgExpr::Expr(expr),
            ..
        } => output_expr_references_name(expr, name),
        _ => false,
    })
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

// Reconstructed after workspace loss from the agreed contract and retained
// regression evidence; not a byte-for-byte restoration of the lost patch.
pub(crate) const JOINT_RESOLUTION_WRAPPER: &str = "__passant_joint_resolution";

pub(crate) fn is_joint_resolution_wrapped_select(select: &Select) -> bool {
    matches!(select.from.first(), Some(TableWithJoins {
        relation: TableFactor::Derived { alias: Some(alias), .. }, joins, ..
    }) if alias.name.value == JOINT_RESOLUTION_WRAPPER && joins.is_empty())
}

fn fresh_abort_identifier(used: &mut HashSet<String>, stem: &str) -> String {
    let mut name = stem.to_owned();
    let mut suffix = 0;
    while !used.insert(name.to_ascii_lowercase()) {
        suffix += 1;
        name = format!("{stem}_{suffix}");
    }
    name
}

#[cfg(test)]
fn wrap_select_with_retry(
    inner: Select,
    policies: Vec<(usize, Expr)>,
) -> Result<Select, RewriteError> {
    let actions = policies
        .into_iter()
        .map(
            |(policy_index, filter)| super::plan::PolicyResolutionAction::Retry {
                policy_index,
                filter,
            },
        )
        .collect::<Vec<_>>();
    wrap_select_with_resolution_stages(
        inner,
        &actions,
        &crate::resolution_registry::ResolutionRegistry::default(),
    )
}

/// Resolve already-computed policy evidence at one candidate boundary. Hidden
/// evidence is not part of the public projection and never becomes a repair arg.
/// CASE dependencies, rather than action order alone, enforce effect precedence.
pub(crate) fn wrap_select_with_resolution_stages(
    mut inner: Select,
    actions: &[super::plan::PolicyResolutionAction],
    registry: &crate::resolution_registry::ResolutionRegistry,
) -> Result<Select, RewriteError> {
    use super::plan::PolicyResolutionAction as A;
    use crate::rewriter::expr::and_expr;
    use crate::sql::{binary_comparison, int_literal, null_literal, scalar_subquery};
    use sqlparser::ast::{BinaryOperator, UnaryOperator};
    ensure_projection_aliases(&mut inner);
    let names = inner
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
            SelectItem::UnnamedExpr(Expr::Identifier(name)) => Some(name.clone()),
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => parts.last().cloned(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            RewriteError::unsupported_statement(
                "joint resolution requires explicit projected columns",
            )
        })?;
    let unique = names
        .iter()
        .map(|name| name.value.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    if names.is_empty() || unique.len() != names.len() {
        return Err(RewriteError::unsupported_statement(
            "joint resolution requires unique non-empty output columns",
        ));
    }
    // Includes expanded wildcard/schema identifiers and hidden evidence refs.
    let mut sql = inner.to_string();
    for action in actions {
        let filter = match action {
            A::Filter { filter, .. }
            | A::TupleUdf { filter, .. }
            | A::RelationUdf { filter, .. }
            | A::Retry { filter, .. }
            | A::Ui { filter, .. } => filter,
        };
        sql.push(' ');
        sql.push_str(&filter.to_string());
    }
    let tokens = sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::GenericDialect {}, &sql)
        .tokenize()
        .map_err(|_| {
            RewriteError::unsupported_statement("cannot allocate resolution identifiers")
        })?;
    let mut used = tokens
        .into_iter()
        .filter_map(|token| match token {
            sqlparser::tokenizer::Token::Word(word) => Some(word.value.to_ascii_lowercase()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let input = fresh_abort_identifier(&mut used, "__passant_candidates");
    let repaired = fresh_abort_identifier(&mut used, "__passant_repaired");
    let resolved = fresh_abort_identifier(&mut used, "__passant_resolved");
    let mut kill = Vec::new();
    let mut remove = Vec::new();
    let mut retries = std::collections::BTreeMap::<usize, Vec<String>>::new();
    let mut udfs: Vec<(bool, String, Vec<String>)> = Vec::new();
    for action in actions {
        let (filter, kind) = match action {
            A::Filter { filter, .. } => (filter, 0),
            A::TupleUdf {
                filter, udf_name, ..
            } if udf_name.eq_ignore_ascii_case(PASSANT_KILL_UDF) => (filter, 1),
            A::Retry { filter, .. } => (filter, 2),
            A::TupleUdf { filter, .. } => (filter, 3),
            A::RelationUdf { filter, .. } => (filter, 4),
            A::Ui { .. } => {
                return Err(RewriteError::unsupported_statement(
                    "UI ordering with joint resolution is not specified",
                ));
            }
        };
        let pass = fresh_abort_identifier(&mut used, PASS_COLUMN);
        inner.projection.push(alias_expr(filter.clone(), &pass));
        match kind {
            0 => remove.push(pass),
            1 => kill.push(pass),
            2 => {
                if let A::Retry { policy_index, .. } = action {
                    retries.entry(*policy_index).or_default().push(pass);
                }
            }
            _ => {
                let name = match action {
                    A::TupleUdf { udf_name, .. } | A::RelationUdf { udf_name, .. } => udf_name,
                    _ => unreachable!(),
                };
                if let Some((relation, _, columns)) = udfs
                    .iter_mut()
                    .find(|(_, existing, _)| existing.eq_ignore_ascii_case(name))
                {
                    if *relation != (kind == 4) {
                        return Err(RewriteError::unsupported_statement(
                            "one resolution function cannot be both tuple and relation UDF",
                        ));
                    }
                    columns.push(pass);
                } else {
                    udfs.push((kind == 4, name.clone(), vec![pass]));
                }
            }
        }
    }
    let col = |name: &str| qualified_column(&input, name);
    let public_col =
        |name: &Ident| Expr::CompoundIdentifier(vec![Ident::new(&input), name.clone()]);
    let all = |exprs: Vec<Expr>| {
        exprs
            .into_iter()
            .reduce(and_expr)
            .unwrap_or_else(|| bool_literal(true))
    };
    let not = |expr: Expr| Expr::UnaryOp {
        op: UnaryOperator::Not,
        expr: Box::new(Expr::Nested(Box::new(expr))),
    };
    let any = |condition: Expr| {
        binary_comparison(
            function_call(
                "max",
                vec![case_when(condition, int_literal(1), int_literal(0))],
            ),
            BinaryOperator::Eq,
            int_literal(1),
        )
    };
    let survivors = all(kill
        .iter()
        .chain(&remove)
        .map(|name| Expr::IsTrue(Box::new(col(name))))
        .collect());
    let retry_parts = retries
        .iter()
        .map(|(index, columns)| {
            let pass = all(columns.iter().map(|name| col(name)).collect());
            case_when(
                any(and_expr(
                    survivors.clone(),
                    Expr::IsNotTrue(Box::new(Expr::Nested(Box::new(pass)))),
                )),
                string_literal(&index.to_string()),
                null_literal(),
            )
        })
        .collect::<Vec<_>>();
    let mut concat_args = vec![string_literal(",")];
    concat_args.extend(retry_parts);
    let payload = function_call("concat_ws", concat_args);
    let retry_guard = if retries.is_empty() {
        bool_literal(true)
    } else {
        case_when(
            binary_comparison(payload.clone(), BinaryOperator::NotEq, string_literal("")),
            function_call("passant_retry_many", vec![payload]),
            bool_literal(true),
        )
    };
    let guard = if kill.is_empty() && retries.is_empty() {
        bool_literal(true)
    } else {
        let aggregate = if kill.is_empty() {
            retry_guard
        } else {
            let pass = all(kill.iter().map(|name| col(name)).collect());
            case_when(
                any(not(pass)),
                function_call(PASSANT_KILL_UDF, vec![]),
                retry_guard,
            )
        };
        scalar_subquery(aggregate, &input)
    };
    let gated = |expr| case_when(guard.clone(), expr, bool_literal(false));
    let from = || {
        vec![TableWithJoins {
            relation: table_factor(&input),
            joins: vec![],
        }]
    };
    let projection = || {
        names
            .iter()
            .map(|name| SelectItem::ExprWithAlias {
                expr: public_col(name),
                alias: name.clone(),
            })
            .collect::<Vec<_>>()
    };
    if udfs.len() > 1 {
        for (_, name, _) in &udfs {
            if registry.registration_order(name).is_none() {
                return Err(RewriteError::unsupported_statement(format!(
                    "resolution function {name} has no definition registration order"
                )));
            }
        }
        udfs.sort_by_key(|(_, name, _)| registry.registration_order(name).unwrap());
        return wrap_ordered_resolution_effects(
            inner, &input, &names, &udfs, guard, survivors, &mut used,
        );
    }
    let mut ctes = vec![cte(&input, query_from_select(inner))];
    let body = match udfs.pop() {
        None => SetExpr::Select(Box::new(grouped_select(
            projection(),
            from(),
            Some(gated(survivors)),
            vec![],
        ))),
        Some((true, name, columns)) => {
            let pass = all(columns.iter().map(|name| col(name)).collect());
            // Preserve nullable bool_or: all-unknown retains the legacy strict
            // callback skip, rather than introducing a definite false effect.
            let violation = function_call(
                "bool_or",
                vec![case_when(survivors.clone(), not(pass), null_literal())],
            );
            let effect = scalar_subquery(
                case_when(
                    guard.clone(),
                    case_when(
                        any(survivors.clone()),
                        function_call(&name, vec![violation]),
                        bool_literal(true),
                    ),
                    bool_literal(false),
                ),
                &input,
            );
            let filter = case_when(
                guard.clone(),
                case_when(survivors, effect, bool_literal(false)),
                bool_literal(false),
            );
            SetExpr::Select(Box::new(grouped_select(
                projection(),
                from(),
                Some(filter),
                vec![],
            )))
        }
        Some((false, name, columns)) => {
            let pass = all(columns.iter().map(|name| col(name)).collect());
            let passing = grouped_select(
                projection(),
                from(),
                Some(gated(and_expr(survivors.clone(), pass.clone()))),
                vec![],
            );
            let failing = and_expr(survivors, not(pass));
            let call = function_call(&name, names.iter().map(public_col).collect());
            let repair = grouped_select(
                vec![alias_expr(
                    case_when(
                        guard.clone(),
                        case_when(failing.clone(), call, null_literal()),
                        null_literal(),
                    ),
                    &resolved,
                )],
                from(),
                Some(gated(failing)),
                vec![],
            );
            ctes.push(cte(&repaired, query_from_select(repair)));
            let repaired_projection = if names.len() == 1 {
                vec![SelectItem::ExprWithAlias {
                    expr: qualified_column(&repaired, &resolved),
                    alias: names[0].clone(),
                }]
            } else {
                names
                    .iter()
                    .map(|name| SelectItem::ExprWithAlias {
                        expr: Expr::CompoundIdentifier(vec![
                            Ident::new(&repaired),
                            Ident::new(&resolved),
                            name.clone(),
                        ]),
                        alias: name.clone(),
                    })
                    .collect()
            };
            let repaired_select = grouped_select(
                repaired_projection,
                vec![TableWithJoins {
                    relation: table_factor(&repaired),
                    joins: vec![],
                }],
                Some(is_not_null(qualified_column(&repaired, &resolved))),
                vec![],
            );
            SetExpr::SetOperation {
                op: SetOperator::Union,
                set_quantifier: SetQuantifier::All,
                left: Box::new(SetExpr::Select(Box::new(passing))),
                right: Box::new(SetExpr::Select(Box::new(repaired_select))),
            }
        }
    };
    let mut outer = empty_select();
    outer.projection = vec![SelectItem::Wildcard(WildcardAdditionalOptions::default())];
    outer.from = vec![TableWithJoins {
        relation: TableFactor::Derived {
            lateral: false,
            subquery: Box::new(with_ctes(ctes, body)),
            alias: Some(crate::sql::table_alias(JOINT_RESOLUTION_WRAPPER)),
        },
        joins: vec![],
    }];
    Ok(outer)
}

/// Original pass evidence is materialized once and carried unchanged. Only the
/// visible values are replaced by each repair; later policy predicates are not
/// evaluated again on those repaired values.
fn wrap_ordered_resolution_effects(
    inner: Select,
    input: &str,
    names: &[Ident],
    udfs: &[(bool, String, Vec<String>)],
    guard: Expr,
    survivors: Expr,
    used: &mut HashSet<String>,
) -> Result<Select, RewriteError> {
    use crate::rewriter::expr::and_expr;
    use crate::sql::{binary_comparison, int_literal, null_literal, scalar_subquery};
    use sqlparser::ast::{BinaryOperator, CteAsMaterialized, UnaryOperator};
    let materialized = |name: &str, query: Query| {
        let mut result = cte(name, query);
        result.materialized = Some(CteAsMaterialized::Materialized);
        result
    };
    let from = |name: &str| {
        vec![TableWithJoins {
            relation: table_factor(name),
            joins: vec![],
        }]
    };
    let flags = udfs
        .iter()
        .flat_map(|(_, _, columns)| columns.iter().cloned())
        .collect::<Vec<_>>();
    let public = |relation: &str| {
        names
            .iter()
            .map(|name| SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(vec![Ident::new(relation), name.clone()]),
                alias: name.clone(),
            })
            .collect::<Vec<_>>()
    };
    let evidence = |relation: &str| {
        flags
            .iter()
            .map(|flag| alias_expr(qualified_column(relation, flag), flag))
            .collect::<Vec<_>>()
    };
    let all_columns = |relation: &str| {
        let mut columns = public(relation);
        columns.extend(evidence(relation));
        columns
    };
    let pass = |relation: &str, columns: &[String]| {
        columns
            .iter()
            .map(|name| qualified_column(relation, name))
            .reduce(and_expr)
            .unwrap_or_else(|| bool_literal(true))
    };
    let not = |expr: Expr| Expr::UnaryOp {
        op: UnaryOperator::Not,
        expr: Box::new(Expr::Nested(Box::new(expr))),
    };
    let mut ctes = vec![materialized(input, query_from_select(inner))];
    let original = fresh_abort_identifier(used, "__passant_original_failures");
    ctes.push(materialized(
        &original,
        query_from_select(grouped_select(
            all_columns(input),
            from(input),
            Some(case_when(guard, survivors, bool_literal(false))),
            vec![],
        )),
    ));
    let mut current = original.clone();
    for (relation, function, columns) in udfs {
        let next = fresh_abort_identifier(used, "__passant_effect_stage");
        if *relation {
            // Freeze the aggregate selection on original surviving candidates;
            // consume the preceding stage before invoking this relation effect.
            let frozen_violation = scalar_subquery(
                function_call("bool_or", vec![not(pass(&original, columns))]),
                &original,
            );
            let effect_name = fresh_abort_identifier(used, "__passant_relation_effect");
            let effect_column = fresh_abort_identifier(used, "__passant_effect_allowed");
            let nonempty = binary_comparison(
                function_call("max", vec![int_literal(1)]),
                BinaryOperator::Eq,
                int_literal(1),
            );
            let effect = case_when(
                nonempty,
                function_call(function, vec![frozen_violation]),
                bool_literal(true),
            );
            ctes.push(materialized(
                &effect_name,
                query_from_select(grouped_select(
                    vec![alias_expr(effect, &effect_column)],
                    from(&current),
                    None,
                    vec![],
                )),
            ));
            ctes.push(materialized(
                &next,
                query_from_select(grouped_select(
                    all_columns(&current),
                    from(&current),
                    Some(scalar_subquery(
                        qualified_column(&effect_name, &effect_column),
                        &effect_name,
                    )),
                    vec![],
                )),
            ));
        } else {
            let current_pass = pass(&current, columns);
            let passing = grouped_select(
                all_columns(&current),
                from(&current),
                Some(current_pass.clone()),
                vec![],
            );
            let called = fresh_abort_identifier(used, "__passant_called_repair");
            let value = fresh_abort_identifier(used, "__passant_repaired_value");
            let failing = not(current_pass);
            let args = names
                .iter()
                .map(|name| Expr::CompoundIdentifier(vec![Ident::new(&current), name.clone()]))
                .collect();
            let mut call_columns = vec![alias_expr(
                case_when(
                    failing.clone(),
                    function_call(function, args),
                    null_literal(),
                ),
                &value,
            )];
            call_columns.extend(evidence(&current));
            ctes.push(materialized(
                &called,
                query_from_select(grouped_select(
                    call_columns,
                    from(&current),
                    Some(failing),
                    vec![],
                )),
            ));
            let mut repaired = names
                .iter()
                .map(|name| SelectItem::ExprWithAlias {
                    expr: if names.len() == 1 {
                        qualified_column(&called, &value)
                    } else {
                        Expr::CompoundIdentifier(vec![
                            Ident::new(&called),
                            Ident::new(&value),
                            name.clone(),
                        ])
                    },
                    alias: name.clone(),
                })
                .collect::<Vec<_>>();
            repaired.extend(evidence(&called));
            let repaired = grouped_select(
                repaired,
                from(&called),
                Some(is_not_null(qualified_column(&called, &value))),
                vec![],
            );
            let mut stage_query = query_from_select(empty_select());
            stage_query.body = Box::new(SetExpr::SetOperation {
                op: SetOperator::Union,
                set_quantifier: SetQuantifier::All,
                left: Box::new(SetExpr::Select(Box::new(passing))),
                right: Box::new(SetExpr::Select(Box::new(repaired))),
            });
            ctes.push(materialized(&next, stage_query));
        }
        current = next;
    }
    let body = grouped_select(public(&current), from(&current), None, vec![]);
    let mut outer = empty_select();
    outer.projection = vec![SelectItem::Wildcard(WildcardAdditionalOptions::default())];
    outer.from = vec![TableWithJoins {
        relation: TableFactor::Derived {
            lateral: false,
            subquery: Box::new(with_ctes(ctes, SetExpr::Select(Box::new(body)))),
            alias: Some(crate::sql::table_alias(JOINT_RESOLUTION_WRAPPER)),
        },
        joins: vec![],
    }];
    Ok(outer)
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
        let kill_filter = passant_kill_tuple_filter(T1_CTE, PASS_COLUMN);
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
        assert!(sql.contains("max(CASE"));
        assert!(!sql.contains("OR CASE WHEN NOT"));
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
        remap_query_order_by_after_kill_wrap(&mut query, None);

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
    #[test]
    fn retry_stage_preserves_quotes_and_reserves_projected_helper_names() {
        let mut inner = empty_select();
        inner.projection = vec![
            SelectItem::ExprWithAlias {
                expr: qualified_column("a", "x"),
                alias: Ident::with_quote('"', "A B"),
            },
            alias_expr(qualified_column("a", "y"), PASS_COLUMN),
        ];
        inner.from = vec![TableWithJoins {
            relation: table_factor("a"),
            joins: vec![],
        }];
        let wrapped = wrap_select_with_retry(inner, vec![(7, bool_literal(false))]).unwrap();
        let sql = render_statement(
            &crate::sql::statement_from_query(query_from_select(wrapped)),
            None,
        );
        assert!(
            sql.contains("__passant_candidates.\"A B\" AS \"A B\""),
            "{sql}"
        );
        assert!(sql.contains("false AS __passant_policy_pass_1"), "{sql}");
        assert!(sql.contains("passant_retry_many"), "{sql}");
        assert!(sql.contains("THEN '7' ELSE NULL"), "{sql}");
    }
}
