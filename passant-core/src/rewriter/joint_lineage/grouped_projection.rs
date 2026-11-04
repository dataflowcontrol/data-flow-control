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
    *query.body = SetExpr::Select(select);
    true
}

fn grouped_projection(query: &Query, registry: &AggregateRegistry) -> Option<Box<Select>> {
    // Moving ORDER/LIMIT or CTE boundaries can change the contributing rows.
    let mut shape = query.clone();
    *shape.body = SetExpr::Select(Box::new(crate::sql::empty_select()));
    if shape != crate::sql::query_from_select(crate::sql::empty_select()) {
        return None;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    if select_is_aggregation(select, registry) {
        // Keep only a global aggregate's FROM/WHERE. Output aliases in a
        // retained clause can change binding when its projection is replaced.
        let mut shape = select.as_ref().clone();
        shape.select_token = crate::sql::empty_select().select_token;
        shape.projection.clear();
        shape.from.clear();
        shape.selection = None;
        if shape != crate::sql::empty_select() || !safe_projection(&select.projection) {
            return None;
        }
        let aliases: HashSet<_> = select
            .projection
            .iter()
            .filter_map(|item| {
                if let SelectItem::ExprWithAlias { alias, .. } = item {
                    Some(alias.value.to_ascii_lowercase())
                } else {
                    None
                }
            })
            .collect();
        let mut retained = select.as_ref().clone();
        retained.projection.clear();
        let alias_reference = visit_expressions(&retained, |expr| {
            if let Expr::Identifier(id) = expr
                && aliases.contains(&id.value.to_ascii_lowercase())
            {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        return alias_reference.is_continue().then(|| select.clone());
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
            let alias = match expr {
                Expr::Identifier(id) => id.clone(),
                Expr::CompoundIdentifier(parts) => parts.last()?.clone(),
                _ => return None,
            };
            *item = SelectItem::ExprWithAlias {
                expr: expr.clone(),
                alias,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::SqlDialect;

    fn query(sql: &str) -> Query {
        let sqlparser::ast::Statement::Query(query) = crate::parser::parse_query(sql).unwrap()
        else {
            panic!("expected query");
        };
        *query
    }

    #[test]
    fn flatten_pure_projection_keeps_global_aggregate() {
        let mut q = query(
            "SELECT v.body AS body FROM (SELECT CAST(list(id) AS VARCHAR) AS body FROM cases) AS v",
        );
        assert!(flatten(
            &mut q,
            &AggregateRegistry::for_dialect(SqlDialect::DuckDb)
        ));
        assert_eq!(
            q.to_string(),
            "SELECT CAST(list(id) AS VARCHAR) AS body FROM cases"
        );
    }

    #[test]
    fn preserve_quoted_output_alias() {
        let mut q = query(
            "SELECT v.\"to\" FROM (SELECT CAST(list(id) AS VARCHAR) AS \"to\" FROM cases) AS v",
        );
        assert!(flatten(
            &mut q,
            &AggregateRegistry::for_dialect(SqlDialect::DuckDb)
        ));
        assert_eq!(
            q.to_string(),
            "SELECT CAST(list(id) AS VARCHAR) AS \"to\" FROM cases"
        );
    }

    #[test]
    fn preserve_cardinality_and_unsupported_boundaries() {
        for sql in [
            "SELECT 1 AS x FROM (SELECT count(*) AS n FROM cases) AS v",
            "SELECT v.body AS body FROM (SELECT CAST(list(id) AS VARCHAR) AS body FROM cases) AS v WHERE v.body IS NOT NULL",
            "SELECT random() AS x, v.body AS body FROM (SELECT CAST(list(id) AS VARCHAR) AS body FROM cases) AS v",
            "SELECT v.body AS body FROM (SELECT category, CAST(list(id) AS VARCHAR) AS body FROM cases GROUP BY 1) AS v",
            "SELECT v.body AS body FROM (SELECT CAST(list(id) AS VARCHAR) AS body FROM cases HAVING body IS NOT NULL) AS v",
            "SELECT v.body AS body FROM (SELECT CAST(list(id) AS VARCHAR) AS body FROM cases LIMIT 1) AS v",
            "SELECT v.body AS body FROM (SELECT 'Discrimination' AS selected_category, CAST(list(id) AS VARCHAR) AS body FROM cases WHERE category=selected_category) AS v",
            "SELECT v.body AS body FROM (SELECT CAST(list(id) AS VARCHAR) AS body FROM cases QUALIFY row_number() OVER () = 1) AS v",
        ] {
            let mut q = query(sql);
            let original = q.clone();
            assert!(
                !flatten(&mut q, &AggregateRegistry::for_dialect(SqlDialect::DuckDb)),
                "{sql}"
            );
            assert_eq!(q, original);
        }
    }
}
