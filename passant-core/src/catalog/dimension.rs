//! Conservative, non-executing binding for DIMENSION SELECT dependencies.
use super::TableCatalog;
use crate::{
    ErrorKind, RewriteError,
    identifiers::{ColumnName, TableKey},
};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, JoinConstraint, JoinOperator, Query,
    SelectItem, SetExpr, Statement, TableFactor, Visit, Visitor,
};
use std::{collections::HashSet, ops::ControlFlow};

#[derive(Default)]
struct Relation {
    qualifiers: HashSet<TableKey>,
    columns: HashSet<String>,
}

pub(super) fn bound_projection(
    catalog: &TableCatalog,
    sql: &str,
) -> Result<Vec<String>, RewriteError> {
    let Statement::Query(query) = crate::parser::parse_query(sql)? else {
        return Err(unsupported("requires a SELECT query"));
    };
    bind_query(catalog, &query)
}

fn unsupported(detail: &str) -> RewriteError {
    RewriteError::unsupported_statement(format!("DIMENSION catalog binding {detail}"))
}
fn unknown(kind: ErrorKind, detail: String) -> RewriteError {
    RewriteError::catalog_with_context(
        kind,
        detail,
        None,
        None,
        None,
        Some("dimension_catalog_validation"),
    )
}

fn bind_query(catalog: &TableCatalog, query: &Query) -> Result<Vec<String>, RewriteError> {
    if query.with.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
    {
        return Err(unsupported(
            "does not yet support CTEs, locking, or dialect-specific query clauses",
        ));
    }
    let (relations, outputs) = match query.body.as_ref() {
        SetExpr::Query(inner) => {
            let outputs = bind_query(catalog, inner)?;
            (
                vec![Relation {
                    columns: outputs.iter().cloned().collect(),
                    ..Default::default()
                }],
                outputs,
            )
        }
        SetExpr::Select(select) => {
            if select.into.is_some()
                || !select.lateral_views.is_empty()
                || select.connect_by.is_some()
            {
                return Err(unsupported(
                    "does not support INTO, lateral views or hierarchical queries",
                ));
            }
            let mut relations = Vec::new();
            for from in &select.from {
                relations.push(bind_table(catalog, &from.relation)?);
                for join in &from.joins {
                    let constraint = match &join.join_operator {
                        JoinOperator::Inner(c)
                        | JoinOperator::LeftOuter(c)
                        | JoinOperator::RightOuter(c)
                        | JoinOperator::FullOuter(c) => Some(c),
                        JoinOperator::CrossJoin => None,
                        _ => {
                            return Err(unsupported(
                                "supports INNER/LEFT/RIGHT/FULL ON joins and CROSS JOIN only",
                            ));
                        }
                    };
                    if constraint
                        .is_some_and(|c| !matches!(c, JoinConstraint::On(_) | JoinConstraint::None))
                    {
                        return Err(unsupported("does not yet bind NATURAL or USING joins"));
                    }
                    relations.push(bind_table(catalog, &join.relation)?);
                    // ON can see only the accumulated left side and this right side,
                    // never a relation introduced by a later JOIN.
                    if let Some(JoinConstraint::On(expr)) = constraint {
                        check(expr, &relations, &HashSet::new())?;
                    }
                }
            }
            // FROM dependencies and ON predicates were checked in their own scopes.
            // Do not revisit ON with the final scope: later relations can both hide
            // an invalid forward reference and make an earlier valid name ambiguous.
            let mut final_expressions = select.as_ref().clone();
            final_expressions.from.clear();
            check(&final_expressions, &relations, &HashSet::new())?;
            let mut outputs = Vec::new();
            for projection in &select.projection {
                let name = match projection {
                    SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                    SelectItem::UnnamedExpr(Expr::Identifier(ident)) => ident.value.clone(),
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                        parts.last().unwrap().value.clone()
                    }
                    _ => {
                        return Err(unsupported(
                            "requires explicitly named projection expressions and does not expand wildcards",
                        ));
                    }
                };
                let name = ColumnName::new(name).key();
                if outputs.contains(&name) {
                    return Err(unsupported("requires unambiguous output column names"));
                }
                outputs.push(name);
            }
            (relations, outputs)
        }
        _ => return Err(unsupported("does not yet bind set operations or VALUES")),
    };
    let output_names = outputs.iter().cloned().collect();
    check(&query.order_by, &relations, &output_names)?;
    check(&query.limit, &relations, &HashSet::new())?;
    check(&query.limit_by, &relations, &HashSet::new())?;
    check(&query.offset, &relations, &HashSet::new())?;
    check(&query.fetch, &relations, &HashSet::new())?;
    Ok(outputs)
}

fn bind_table(catalog: &TableCatalog, factor: &TableFactor) -> Result<Relation, RewriteError> {
    let TableFactor::Table {
        name,
        alias,
        args,
        version,
        ..
    } = factor
    else {
        return Err(unsupported(
            "does not yet bind derived tables, table functions, or lateral/correlated scopes",
        ));
    };
    if args.is_some() || version.is_some() || alias.as_ref().is_some_and(|a| !a.columns.is_empty())
    {
        return Err(unsupported(
            "does not yet bind table arguments, temporal versions, or column alias lists",
        ));
    }
    let Some(columns) = catalog.columns(&name.to_string()) else {
        return Err(unknown(
            ErrorKind::UnknownTable,
            format!("Dimension dependency table '{name}' does not exist in the explicit catalog"),
        ));
    };
    let qualifiers = if let Some(alias) = alias {
        HashSet::from([TableKey::new(&alias.name.value)])
    } else {
        HashSet::from([
            TableKey::new(&name.to_string()),
            TableKey::new(&name.0.last().unwrap().value),
        ])
    };
    Ok(Relation {
        qualifiers,
        columns: columns.iter().map(|c| ColumnName::new(c).key()).collect(),
    })
}

fn check<T: Visit>(
    node: &T,
    relations: &[Relation],
    outputs: &HashSet<String>,
) -> Result<(), RewriteError> {
    struct Binder<'a> {
        relations: &'a [Relation],
        outputs: &'a HashSet<String>,
    }
    impl Binder<'_> {
        fn column(&self, qualifier: Option<TableKey>, column: &str) -> Result<(), RewriteError> {
            let key = ColumnName::new(column).key();
            if qualifier.is_none() && self.outputs.contains(&key) {
                return Ok(());
            }
            let matching: Vec<_> = self
                .relations
                .iter()
                .filter(|r| qualifier.as_ref().is_none_or(|q| r.qualifiers.contains(q)))
                .collect();
            let count = matching.iter().filter(|r| r.columns.contains(&key)).count();
            if count == 1 {
                return Ok(());
            }
            Err(unknown(
                ErrorKind::UnknownColumn,
                format!(
                    "Dimension dependency column '{}{}' is {}",
                    qualifier
                        .map(|q| format!("{}.", q.as_str()))
                        .unwrap_or_default(),
                    column,
                    if count == 0 {
                        "not bound to an explicit catalog column"
                    } else {
                        "ambiguous across input relations"
                    }
                ),
            ))
        }
        fn qualifier(&self, name: &sqlparser::ast::ObjectName) -> Result<(), RewriteError> {
            let key = TableKey::new(&name.to_string());
            if self
                .relations
                .iter()
                .filter(|r| r.qualifiers.contains(&key))
                .count()
                == 1
            {
                Ok(())
            } else {
                Err(unknown(
                    ErrorKind::UnknownTable,
                    format!("Dimension wildcard qualifier '{name}' is not bound unambiguously"),
                ))
            }
        }
    }
    impl Visitor for Binder<'_> {
        type Break = RewriteError;
        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            ControlFlow::Break(unsupported(
                "does not yet bind nested expression/correlated subqueries",
            ))
        }
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
            let result = match expr {
                Expr::Identifier(ident) => self.column(None, &ident.value),
                Expr::CompoundIdentifier(parts) if parts.len() >= 2 => self.column(
                    Some(TableKey::new(
                        &parts[..parts.len() - 1]
                            .iter()
                            .map(|i| i.to_string())
                            .collect::<Vec<_>>()
                            .join("."),
                    )),
                    &parts.last().unwrap().value,
                ),
                Expr::QualifiedWildcard(name, _) => self.qualifier(name),
                Expr::Function(function) => {
                    if let FunctionArguments::List(args) = &function.args {
                        for arg in &args.args {
                            let arg = match arg {
                                FunctionArg::Unnamed(arg)
                                | FunctionArg::Named { arg, .. }
                                | FunctionArg::ExprNamed { arg, .. } => arg,
                            };
                            if let FunctionArgExpr::QualifiedWildcard(name) = arg
                                && let Err(error) = self.qualifier(name)
                            {
                                return ControlFlow::Break(error);
                            }
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            };
            match result {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            }
        }
    }
    match node.visit(&mut Binder { relations, outputs }) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalog() -> TableCatalog {
        let mut c = TableCatalog::default();
        c.register_table("s.users", vec!["id".into(), "display name".into()]);
        c.register_table("roles", vec!["id".into(), "enabled".into()]);
        c
    }
    #[test]
    fn dependencies_are_checked_before_output_aliases_are_trusted() {
        let c = catalog();
        for sql in [
            "SELECT ghost.id AS id FROM ghost",
            "SELECT u.missing AS id FROM s.users u",
            "SELECT u.id AS id FROM s.users u WHERE u.missing > 0",
            "SELECT u.id AS id FROM s.users u JOIN roles r ON u.id=r.missing",
            "SELECT u.id AS id FROM s.users u ORDER BY absent",
            "SELECT id AS output FROM s.users u JOIN roles r ON u.id=r.id",
            "SELECT COUNT(ghost.*) AS total FROM s.users",
            "SELECT a.id AS id FROM s.users a JOIN roles b ON c.id=a.id JOIN s.users c ON TRUE",
        ] {
            assert!(bound_projection(&c, sql).is_err(), "{sql}");
        }
    }
    #[test]
    fn proven_physical_scopes_and_output_order_aliases_bind() {
        let c = catalog();
        for sql in [
            "(SELECT id FROM s.users WHERE id > 0)",
            "SELECT u.\"display name\" AS label FROM \"s\".\"users\" AS u ORDER BY label",
            "SELECT u.id AS uid FROM s.users u JOIN roles r ON u.id=r.id WHERE r.enabled",
            "SELECT MAX(id) AS id FROM s.users HAVING MAX(id) > 0",
            "SELECT 1 AS id",
            "SELECT a.id AS id FROM s.users a JOIN roles b ON b.id=a.id JOIN s.users c ON c.id=b.id",
            "SELECT a.id AS id FROM s.users a JOIN roles b ON enabled JOIN roles c ON c.id=b.id",
        ] {
            assert!(bound_projection(&c, sql).is_ok(), "{sql}");
        }
    }
    #[test]
    fn unknown_scope_shapes_do_not_silently_skip_dependency_validation() {
        let c = catalog();
        for sql in [
            "WITH d AS (SELECT id FROM ghost) SELECT id FROM d",
            "SELECT d.id FROM (SELECT id FROM ghost) d",
            "SELECT (SELECT ghost.id FROM ghost) AS id",
            "SELECT u.id FROM s.users u JOIN roles r USING (id)",
        ] {
            assert!(bound_projection(&c, sql).is_err(), "{sql}");
        }
    }
}
