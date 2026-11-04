//! Flatten pure projection wrappers over one grouped input before policy placement.
//! This keeps source aggregates at their contributor boundary and exposes the final
//! sink expressions to the ordinary grouped INSERT planner. Unsupported wrappers
//! retain the existing fail-closed lineage path.
use super::*;
use crate::aggregate_registry::AggregateRegistry;

pub(super) fn flatten(query: &mut Query, registry: &AggregateRegistry) -> bool {
    let Some(select) = grouped_projection(query, registry) else {
        return false;
    };
    if let SetExpr::Select(original) = query.body.as_ref()
        && select.as_ref() == original.as_ref()
    {
        return false;
    }
    query.body = Box::new(SetExpr::Select(select));
    true
}

fn grouped_projection(query: &Query, registry: &AggregateRegistry) -> Option<Box<Select>> {
    // Moving ORDER/LIMIT or CTE boundaries can change the contributing rows.
    let mut shape = query.clone();
    shape.body = Box::new(SetExpr::Select(Box::new(crate::sql::empty_select())));
    if shape != crate::sql::query_from_select(crate::sql::empty_select()) {
        return None;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    if select_is_aggregation(select, registry) {
        // The scoped path is a global aggregate. GROUP BY ordinals/aliases and
        // HAVING output aliases need binding-aware remapping before projection
        // substitution; keep those on the established lineage path for now.
        let sqlparser::ast::GroupByExpr::Expressions(groups, modifiers) = &select.group_by else {
            return None;
        };
        return (groups.is_empty()
            && modifiers.is_empty()
            && select.having.is_none()
            && select.distinct.is_none()
            && safe_projection(&select.projection))
        .then(|| select.clone());
    }
    let mut shape = select.as_ref().clone();
    shape.select_token = crate::sql::empty_select().select_token;
    shape.projection.clear();
    shape.from.clear();
    if shape != crate::sql::empty_select() || select.from.len() != 1 {
        return None;
    }
    let table = &select.from[0];
    if !table.joins.is_empty() || !safe_projection(&select.projection) {
        return None;
    }
    let TableFactor::Derived {
        lateral: false,
        subquery,
        alias: Some(alias),
    } = &table.relation
    else {
        return None;
    };
    if !alias.columns.is_empty() {
        return None;
    }
    let mut inner = grouped_projection(subquery, registry)?;
    let mut columns = HashMap::new();
    for item in &inner.projection {
        let (expr, name) = super::super::expr::projection_expr_and_name(item)?;
        let name = name.or_else(|| super::super::expr::projected_column_name(expr))?;
        if columns
            .insert(name.to_ascii_lowercase(), expr.clone())
            .is_some()
        {
            return None;
        }
    }
    let mut projection = select.projection.clone();
    for item in &mut projection {
        // Preserve public names before substituting their defining expressions.
        if let SelectItem::UnnamedExpr(expr) = item {
            let name = super::super::expr::projected_column_name(expr)?;
            *item = SelectItem::ExprWithAlias {
                expr: expr.clone(),
                alias: Ident::new(name),
            };
        }
        let result = visit_expressions_mut(item, |expr| {
            let name = match expr {
                Expr::Identifier(name) => Some(name.value.to_ascii_lowercase()),
                Expr::CompoundIdentifier(parts)
                    if parts.len() == 2
                        && parts[0].value.eq_ignore_ascii_case(&alias.name.value) =>
                {
                    Some(parts[1].value.to_ascii_lowercase())
                }
                Expr::CompoundIdentifier(_) => return ControlFlow::Break(()),
                _ => None,
            };
            if let Some(name) = name {
                let Some(replacement) = columns.get(&name) else {
                    return ControlFlow::Break(());
                };
                *expr = replacement.clone();
            }
            ControlFlow::Continue(())
        });
        if result.is_break() {
            return None;
        }
    }
    inner.projection = projection;
    // An unused global aggregate still determines cardinality; do not erase it.
    select_is_aggregation(&inner, registry).then_some(inner)
}

fn safe_projection(items: &[SelectItem]) -> bool {
    // Expansion can repeat expressions. Do not duplicate volatile or unknown
    // functions, windows, or subqueries. These deterministic functions cover the
    // typed JSON sink adapter without altering the underlying grouped query.
    items.iter().all(|item| {
        visit_expressions(item, |expr| {
            match expr {
                Expr::Function(function) => {
                    if function.over.is_some()
                        || !matches!(
                            function.name.to_string().to_ascii_lowercase().as_str(),
                            "json_object"
                                | "json_type"
                                | "regexp_full_match"
                                | "list"
                                | "array_agg"
                                | "min"
                                | "max"
                                | "sum"
                                | "count"
                                | "bool_and"
                                | "bool_or"
                                | "coalesce"
                        )
                    {
                        return ControlFlow::Break(());
                    }
                }
                Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                    return ControlFlow::Break(());
                }
                _ => {}
            }
            ControlFlow::Continue(())
        })
        .is_continue()
    })
}
