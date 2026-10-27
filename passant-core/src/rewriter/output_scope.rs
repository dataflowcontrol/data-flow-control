//! Statement-output availability for no-SINK `_OUTPUT_` policies.
//! Pure binding: no execution, policy effects, or provenance instrumentation.
use super::{PassantRewriter, RewriteError};
use crate::identifiers::TableKey;
use crate::policy::PolicyIr;
use sqlparser::ast::{Expr, Query, SelectItem, SetExpr, Statement, TableFactor, Visit, Visitor};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::ControlFlow,
    rc::Rc,
};

pub(crate) fn required_columns(policy: &PolicyIr, expr: &Expr) -> HashSet<String> {
    if policy.sink().is_some() {
        return HashSet::new();
    }
    struct Columns(HashSet<String>);
    impl Visitor for Columns {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if let Expr::CompoundIdentifier(parts) = expr
                && parts.len() == 2
                && parts[0].value.eq_ignore_ascii_case("_OUTPUT_")
            {
                self.0.insert(parts[1].value.to_ascii_lowercase());
            }
            ControlFlow::Continue(())
        }
    }
    let mut columns = Columns(HashSet::new());
    let _ = expr.visit(&mut columns);
    columns.0
}

pub(crate) fn has_output_policy(rewriter: &PassantRewriter, index: usize) -> bool {
    rewriter
        .store
        .policy(index)
        .zip(rewriter.store.clone_constraint_ast(index))
        .is_some_and(|(policy, expr)| !required_columns(policy, &expr).is_empty())
}

pub(crate) fn applicable_output_policy(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Option<(usize, String)> {
    let tables = crate::statement_tables::statement_table_keys(statement);
    let sink = crate::statement_tables::statement_sink_key(statement);
    rewriter
        .store
        .policy_indices()
        .into_iter()
        .find_map(|index| {
            if !has_output_policy(rewriter, index) {
                return None;
            }
            let compiled = rewriter.store.compiled(index)?;
            super::policy_expr::compiled_policy_applicability(
                compiled,
                &tables,
                sink.as_ref(),
                false,
            )?;
            Some((
                compiled.original_policy_index,
                compiled.policy.constraint().to_owned(),
            ))
        })
}

pub(crate) fn scoped_rewriter(
    rewriter: &PassantRewriter,
    statement: &Statement,
) -> Result<Option<PassantRewriter>, RewriteError> {
    let tables = crate::statement_tables::statement_table_keys(statement);
    let sink = crate::statement_tables::statement_sink_key(statement);
    let policies: Vec<_> = rewriter
        .store
        .policy_indices()
        .into_iter()
        .filter_map(|index| {
            let compiled = rewriter.store.compiled(index)?;
            super::policy_expr::compiled_policy_applicability(
                compiled,
                &tables,
                sink.as_ref(),
                true,
            )?;
            let policy = rewriter.store.policy(index)?;
            let expr = rewriter.store.clone_constraint_ast(index)?;
            let required = required_columns(policy, &expr);
            (!required.is_empty()).then_some((index, required))
        })
        .collect();
    if policies.is_empty() {
        return Ok(None);
    }
    let outputs = match statement {
        Statement::Query(query) => query_outputs(rewriter, query, &HashMap::new())?,
        // Preserve existing INSERT _OUTPUT_ binding to target columns, not SELECT labels.
        Statement::Insert(insert) => {
            if insert.columns.is_empty() {
                rewriter
                    .catalog
                    .columns(&insert.table_name.to_string())
                    .map(<[String]>::to_vec)
                    .ok_or_else(|| {
                        unsupported("requires an explicit target catalog for INSERT output columns")
                    })?
            } else {
                insert.columns.iter().map(|i| i.value.clone()).collect()
            }
        }
        // Other write forms retain their existing assignment/output semantics.
        _ => return Ok(None),
    };
    let mut counts = HashMap::new();
    for name in outputs {
        *counts.entry(name.to_ascii_lowercase()).or_insert(0usize) += 1;
    }
    let outputs: HashSet<_> = counts.keys().cloned().collect();
    let mut missing = Vec::new();
    for (index, required) in policies {
        if !required.is_subset(&outputs) {
            missing.push(index);
            continue;
        }
        if let Some(column) = required
            .iter()
            .find(|name| counts.get(*name).copied().unwrap_or(0) > 1)
        {
            return Err(RewriteError::unsupported_statement(format!(
                "ambiguous _OUTPUT_ column '{column}': multiple projected expressions share this output name"
            )));
        }
    }
    if missing.is_empty() {
        return Ok(None);
    }
    let mut scoped = rewriter.clone();
    for index in missing {
        scoped.store.deactivate(index);
    }
    Ok(Some(scoped))
}

fn unsupported(detail: &str) -> RewriteError {
    RewriteError::unsupported_statement(format!("_OUTPUT_ schema binding {detail}"))
}

#[derive(Clone)]
struct CteBinding<'a> {
    query: &'a Query,
    aliases: &'a [sqlparser::ast::TableAliasColumnDef],
    outer: HashMap<TableKey, Rc<CteBinding<'a>>>,
    cached: RefCell<Option<Vec<String>>>,
}

type CteBindings<'a> = HashMap<TableKey, Rc<CteBinding<'a>>>;

fn body_has_wildcard(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => select.projection.iter().any(|item| {
            matches!(
                item,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
            )
        }),
        SetExpr::Query(query) => body_has_wildcard(&query.body),
        SetExpr::SetOperation { left, .. } => body_has_wildcard(left),
        _ => false,
    }
}

fn query_outputs<'a>(
    rewriter: &PassantRewriter,
    query: &'a Query,
    outer: &CteBindings<'a>,
) -> Result<Vec<String>, RewriteError> {
    // Explicit public names need no input schema, including unused/opaque CTEs.
    if !body_has_wildcard(&query.body) {
        return body_outputs(rewriter, &query.body, outer);
    }
    let mut ctes = outer.clone();
    if let Some(with) = &query.with {
        if with.recursive {
            return Err(unsupported(
                "does not yet bind recursive CTE wildcard schemas",
            ));
        }
        for cte in &with.cte_tables {
            let binding = CteBinding {
                query: &cte.query,
                aliases: &cte.alias.columns,
                outer: ctes.clone(),
                cached: RefCell::new(None),
            };
            ctes.insert(TableKey::new(&cte.alias.name.value), Rc::new(binding));
        }
    }
    body_outputs(rewriter, &query.body, &ctes)
}

fn body_outputs<'a>(
    rewriter: &PassantRewriter,
    body: &'a SetExpr,
    ctes: &CteBindings<'a>,
) -> Result<Vec<String>, RewriteError> {
    match body {
        SetExpr::Query(query) => query_outputs(rewriter, query, ctes),
        SetExpr::SetOperation {
            left,
            set_quantifier,
            ..
        } => {
            if !matches!(
                set_quantifier,
                sqlparser::ast::SetQuantifier::None
                    | sqlparser::ast::SetQuantifier::All
                    | sqlparser::ast::SetQuantifier::Distinct
            ) {
                return Err(unsupported(
                    "does not yet bind BY NAME set-operation schemas",
                ));
            }
            body_outputs(rewriter, left, ctes)
        }
        SetExpr::Select(select) => {
            let mut outputs = Vec::new();
            for item in &select.projection {
                match item {
                    SelectItem::ExprWithAlias { alias, .. } => outputs.push(alias.value.clone()),
                    SelectItem::UnnamedExpr(expr) => {
                        if let Some(name) = super::expr::projected_column_name(expr) {
                            outputs.push(name);
                        } else {
                            outputs.push(
                                super::joint_lineage::public_output_label(
                                    expr,
                                    rewriter.parse_dialect,
                                )?
                                .ok_or_else(|| unsupported("cannot bind unnamed expression"))?,
                            );
                        }
                    }
                    SelectItem::Wildcard(options) | SelectItem::QualifiedWildcard(_, options) => {
                        if options != &Default::default() {
                            return Err(unsupported("does not yet bind wildcard modifiers"));
                        }
                        let qualifier = match item {
                            SelectItem::QualifiedWildcard(name, _) => {
                                Some(TableKey::new(&name.to_string()))
                            }
                            _ => None,
                        };
                        let mut matched = false;
                        for table in &select.from {
                            for factor in std::iter::once(&table.relation)
                                .chain(table.joins.iter().map(|j| &j.relation))
                            {
                                let (names, aliases) = factor_outputs(rewriter, factor, ctes)?;
                                if qualifier.as_ref().is_none_or(|q| aliases.contains(q)) {
                                    outputs.extend(names);
                                    matched = true;
                                }
                            }
                        }
                        if !matched {
                            return Err(unsupported("cannot resolve wildcard relation"));
                        }
                    }
                }
            }
            Ok(outputs)
        }
        _ => Err(unsupported("requires a SELECT output schema")),
    }
}

fn factor_outputs<'a>(
    rewriter: &PassantRewriter,
    factor: &'a TableFactor,
    ctes: &CteBindings<'a>,
) -> Result<(Vec<String>, HashSet<TableKey>), RewriteError> {
    let (mut outputs, alias, mut names) = match factor {
        TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => {
            let key = TableKey::new(&name.to_string());
            let outputs = if let Some(binding) = ctes.get(&key) {
                let cached = binding.cached.borrow().clone();
                if let Some(outputs) = cached {
                    outputs
                } else {
                    let mut outputs = query_outputs(rewriter, binding.query, &binding.outer)?;
                    for (name, alias) in outputs.iter_mut().zip(binding.aliases) {
                        *name = alias.name.value.clone();
                    }
                    *binding.cached.borrow_mut() = Some(outputs.clone());
                    outputs
                }
            } else {
                rewriter
                    .catalog
                    .columns(&name.to_string())
                    .map(<[String]>::to_vec)
                    .ok_or_else(|| {
                        unsupported("requires an explicit catalog for wildcard tables")
                    })?
            };
            (outputs, alias.as_ref(), HashSet::from([key]))
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => (
            query_outputs(rewriter, subquery, ctes)?,
            alias.as_ref(),
            HashSet::new(),
        ),
        _ => return Err(unsupported("cannot resolve this wildcard table factor")),
    };
    if let Some(alias) = alias {
        names = HashSet::from([TableKey::new(&alias.name.value)]);
        for (name, column) in outputs.iter_mut().zip(&alias.columns) {
            *name = column.name.value.clone();
        }
    }
    let mut unique = HashSet::new();
    if outputs
        .iter()
        .any(|name| !unique.insert(name.to_ascii_lowercase()))
    {
        return Err(unsupported(
            "cannot infer dialect-specific deduplicated names of a wildcard input; use explicit distinct column aliases",
        ));
    }
    Ok((outputs, names))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TableCatalog, parse_policy_text};

    fn rewriter() -> PassantRewriter {
        let mut catalog = TableCatalog::new();
        catalog.register_table("a", vec!["x".into(), "y".into(), "Odd Name".into()]);
        catalog.register_table("target", vec!["x".into(), "y".into()]);
        let mut rewriter = PassantRewriter::with_catalog(catalog);
        rewriter.register_policy(parse_policy_text("SOURCE a CONSTRAINT _OUTPUT_.x > 0 AND _OUTPUT_.y > 0 ON FAIL RETRY('both columns')").unwrap()).unwrap();
        rewriter
    }

    #[test]
    fn all_output_columns_are_required_and_aliases_are_public() {
        let rewriter = rewriter();
        for sql in [
            "SELECT x FROM a",
            "SELECT x AS z, y FROM a",
            "WITH c AS (SELECT x,y FROM a) SELECT x AS z,y FROM c",
        ] {
            let statement = crate::parser::parse_query(sql).unwrap();
            let scoped = scoped_rewriter(&rewriter, &statement).unwrap().unwrap();
            assert!(scoped.store.is_empty(), "{sql}");
            assert_eq!(rewriter.store.active_count(), 1);
        }
        for sql in [
            "SELECT x,y FROM a",
            "SELECT NULL AS x,y FROM a",
            "SELECT a.* FROM a",
            "SELECT q.* FROM a q",
            "WITH c(z,y) AS (SELECT x,y FROM a) SELECT z AS x,y FROM c",
            "SELECT d.* FROM (SELECT x,y FROM a) d",
            "SELECT x,y FROM a UNION ALL SELECT y,x FROM a",
            "INSERT INTO target(x,y) SELECT x AS other,y FROM a",
        ] {
            let statement = crate::parser::parse_query(sql).unwrap();
            assert!(
                scoped_rewriter(&rewriter, &statement).unwrap().is_none(),
                "{sql}"
            );
        }
    }

    #[test]
    fn opaque_unused_cte_does_not_prevent_explicit_missing_output_skip() {
        let rewriter = rewriter();
        for sql in [
            "WITH c AS (SELECT mystery(x) FROM a) SELECT x AS z FROM a",
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<2) SELECT y FROM a",
        ] {
            let statement = crate::parser::parse_query(sql).unwrap();
            assert!(
                scoped_rewriter(&rewriter, &statement)
                    .unwrap()
                    .unwrap()
                    .store
                    .is_empty()
            );
        }
    }

    #[test]
    fn unproven_wildcard_renaming_and_by_name_never_silently_skip() {
        let rewriter = rewriter();
        for sql in [
            "SELECT * FROM (SELECT x,x FROM a) d",
            "WITH c AS (SELECT x,x FROM a) SELECT * FROM c",
            "SELECT y FROM a UNION BY NAME SELECT x FROM a",
        ] {
            let statement = crate::parser::parse_query(sql).unwrap();
            assert!(scoped_rewriter(&rewriter, &statement).is_err(), "{sql}");
        }
    }

    #[test]
    fn skipped_policy_keeps_other_original_identity() {
        let mut rewriter = rewriter();
        rewriter
            .register_policy(
                parse_policy_text("SOURCE a CONSTRAINT _OUTPUT_.x > 0 ON FAIL RETRY('x only')")
                    .unwrap(),
            )
            .unwrap();
        let statement = crate::parser::parse_query("SELECT x FROM a").unwrap();
        let scoped = scoped_rewriter(&rewriter, &statement).unwrap().unwrap();
        assert_eq!(scoped.store.policy_indices(), vec![1]);
        assert_eq!(scoped.store.original_policy_index(1), Some(1));
    }

    #[test]
    fn duplicate_output_names_are_present_not_silently_skipped() {
        let rewriter = rewriter();
        let statement = crate::parser::parse_query("SELECT x,y,1 AS x FROM a").unwrap();
        assert!(scoped_rewriter(&rewriter, &statement).is_err());
        assert!(rewriter.rewrite("SELECT x,y,1 AS x FROM a").is_err());
    }
    #[test]
    fn existing_insert_aggregate_output_endpoint_is_retained() {
        let mut rewriter = rewriter();
        rewriter.store.deactivate(0);
        rewriter
            .register_policy(
                parse_policy_text(
                    "SOURCE a CONSTRAINT MAX(a.y) > 0 AND _OUTPUT_.y > 0 ON FAIL KILL",
                )
                .unwrap(),
            )
            .unwrap();
        let sql = rewriter.rewrite("INSERT INTO target(x,y) WITH selected AS (SELECT * FROM (VALUES (1)) AS v(id)), candidates AS (SELECT a.x,a.y FROM a JOIN selected ON a.x=selected.id), rollup AS (SELECT x,SUM(y) AS y FROM candidates GROUP BY x) SELECT x,y FROM rollup").unwrap();
        assert!(sql.contains("__passant_max_y"), "{sql}");
        assert!(sql.to_ascii_lowercase().contains("passant_kill()"), "{sql}");
    }
    #[test]
    fn partial_push_output_binding_gap_is_explicit_and_missing_output_still_skips() {
        let mut rewriter = rewriter();
        rewriter.store.deactivate(0);
        rewriter
            .register_policy(
                parse_policy_text(
                    "SOURCE a CONSTRAINT _OUTPUT_.x > 0 AND MEDIAN(a.y) > 0 ON FAIL REMOVE",
                )
                .unwrap(),
            )
            .unwrap();
        let error = rewriter.rewrite("SELECT 1 AS x FROM a").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("partial-push policy evaluation does not yet bind final _OUTPUT_"),
            "{error}"
        );
        assert_eq!(
            rewriter.rewrite("SELECT y FROM a").unwrap(),
            "SELECT y FROM a"
        );
    }
}
