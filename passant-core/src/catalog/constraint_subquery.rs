//! Registration-time physical relation checks for SQL subqueries in CONSTRAINT.

use std::{collections::HashSet, ops::ControlFlow, ptr};

use sqlparser::ast::{Expr, Query, TableFactor, Visit, Visitor};

use super::TableCatalog;
use crate::{
    diagnostics::{ErrorKind, RewriteError},
    identifiers::TableKey,
};

#[derive(Default)]
struct QueryScope {
    visible_ctes: HashSet<TableKey>,
    // A nonrecursive WITH item enters scope only after its defining query.
    pending_ctes: Vec<(*const Query, TableKey)>,
}

pub(super) fn validate(
    catalog: &TableCatalog,
    expr: &Expr,
    constraint: &str,
) -> Result<(), RewriteError> {
    struct Relations<'a> {
        catalog: &'a TableCatalog,
        constraint: &'a str,
        queries: Vec<QueryScope>,
    }

    impl Visitor for Relations<'_> {
        type Break = RewriteError;

        fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            let mut scope = QueryScope::default();
            if let Some(with) = &query.with {
                for cte in &with.cte_tables {
                    let name = TableKey::new(&cte.alias.name.to_string());
                    if with.recursive {
                        scope.visible_ctes.insert(name);
                    } else {
                        scope
                            .pending_ctes
                            .push((ptr::from_ref(cte.query.as_ref()), name));
                    }
                }
            }
            self.queries.push(scope);
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
            self.queries.pop();
            if let Some(parent) = self.queries.last_mut()
                && let Some((_, name)) = parent
                    .pending_ctes
                    .iter()
                    .find(|(definition, _)| ptr::eq(*definition, query))
            {
                parent.visible_ctes.insert(name.clone());
            }
            ControlFlow::Continue(())
        }

        fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
            // Derived tables, aliases and table functions are not physical
            // catalog entries. The visitor still traverses their children.
            let TableFactor::Table {
                name, args: None, ..
            } = factor
            else {
                return ControlFlow::Continue(());
            };
            let name_str = name.to_string();
            let key = TableKey::new(&name_str);
            let is_cte = name.0.len() == 1
                && self
                    .queries
                    .iter()
                    .rev()
                    .any(|scope| scope.visible_ctes.contains(&key));
            if self.queries.is_empty() || is_cte || self.catalog.table_exists(&name_str) {
                return ControlFlow::Continue(());
            }
            ControlFlow::Break(RewriteError::catalog_with_context(
                ErrorKind::UnknownTable,
                format!(
                    "CONSTRAINT subquery table '{name_str}' does not exist in the explicit catalog"
                ),
                Some(name_str),
                None,
                Some(self.constraint.to_string()),
                Some("catalog_validation"),
            ))
        }
    }

    match expr.visit(&mut Relations {
        catalog,
        constraint,
        queries: Vec::new(),
    }) {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewriter::PassantRewriter;

    fn register(constraint: &str) -> Result<(), RewriteError> {
        let mut catalog = TableCatalog::new();
        catalog.register_table("target", vec!["id".into()]);
        catalog.register_table("lookup_correct", vec!["id".into()]);
        catalog.register_table("extra_lookup", vec!["id".into()]);
        let mut rewriter = PassantRewriter::with_catalog(catalog);
        rewriter.register_policy_text(&format!(
            "SINK target AS t DIMENSION lookup_correct AS d CONSTRAINT {constraint} ON FAIL RETRY('blocked')"
        ))
    }

    #[test]
    fn rejects_missing_subquery_relations_with_a_valid_dimension() {
        let cases = [
            (
                "EXISTS (SELECT 1 FROM lookup_typo AS d WHERE d.id = t.id)",
                "lookup_typo",
            ),
            (
                "(SELECT MAX(x.id) FROM lookup_typo AS x) > 0",
                "lookup_typo",
            ),
            ("t.id IN (SELECT x.id FROM lookup_typo AS x)", "lookup_typo"),
            (
                "EXISTS (SELECT 1 FROM lookup_correct AS x WHERE EXISTS (SELECT 1 FROM lookup_typo AS x))",
                "lookup_typo",
            ),
            (
                "EXISTS (WITH x AS (SELECT id FROM lookup_typo) SELECT 1 FROM x)",
                "lookup_typo",
            ),
            (
                "EXISTS (WITH x AS (SELECT id FROM later), later AS (SELECT 1 AS id) SELECT 1 FROM x)",
                "later",
            ),
            (
                "EXISTS (WITH x AS (SELECT 1) SELECT 1 FROM missing_schema.x)",
                "missing_schema.x",
            ),
            (
                "EXISTS (SELECT 1 FROM (WITH x AS (SELECT 1) SELECT 1 FROM x) AS q JOIN x ON TRUE)",
                "x",
            ),
        ];
        for (constraint, table) in cases {
            let error = register(constraint).expect_err(constraint);
            assert_eq!(error.kind(), ErrorKind::UnknownTable, "{constraint}");
            let RewriteError::Catalog(details) = error else {
                panic!("expected catalog diagnostic");
            };
            assert_eq!(details.table.as_deref(), Some(table), "{constraint}");
            assert_eq!(details.constraint.as_deref(), Some(constraint));
            assert_eq!(
                details.validation_phase.as_deref(),
                Some("catalog_validation")
            );
        }
    }

    #[test]
    fn accepts_physical_relations_ctes_and_correlated_aliases() {
        let cases = [
            "EXISTS (SELECT 1 FROM lookup_correct AS d WHERE d.id = t.id)",
            "EXISTS (SELECT 1 FROM extra_lookup AS t WHERE t.id = 9)",
            "EXISTS (SELECT 1 FROM unnest([t.id]) AS t(id) WHERE t.id = 1)",
            "EXISTS (SELECT 1 FROM (SELECT t.id AS id) AS t WHERE t.id = 1)",
            "EXISTS (WITH x AS (SELECT id FROM lookup_correct), y AS (SELECT id FROM x) SELECT 1 FROM y WHERE EXISTS (SELECT 1 FROM x WHERE x.id = t.id))",
            "EXISTS (WITH RECURSIVE x(id) AS (SELECT 1 UNION ALL SELECT x.id + 1 FROM x WHERE x.id < 2) SELECT 1 FROM x)",
            "(SELECT t.id) > 0",
        ];
        for constraint in cases {
            register(constraint).unwrap_or_else(|error| panic!("{constraint}: {error}"));
        }
    }
}
