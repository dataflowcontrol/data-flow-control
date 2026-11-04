//! Catalog-backed export/import of row provenance for persisted intermediates.
//!
//! This is the data channel used by the execution adapter, not another policy
//! parser or resolution engine. Base scans export contributor identities;
//! persisted scans import opaque state tokens; CTEs/derived tables carry the
//! same channel through projections, joins, grouping and aggregate windows.
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator, Query, SelectItem,
    SetExpr, Statement, TableAliasColumnDef, TableFactor, Value, WindowType, visit_expressions,
    visit_expressions_mut,
};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;

use crate::aggregate_registry::{AggregateFunctionSource, AggregateRegistry};
use crate::catalog::TableCatalog;
use crate::diagnostics::RewriteError;
use crate::rewriter::PassantRewriter;
use crate::sql::parse_policy_expr_duckdb;

const DEPS: &str = "_dfc_intermediate_dependencies";
const EMPTY: &str = "CAST([] AS STRUCT(t VARCHAR, r BIGINT)[])";

fn error(message: &str) -> RewriteError {
    RewriteError::unsupported_statement(format!("intermediate lineage: {message}"))
}
fn quoted(name: &str) -> String {
    Ident::with_quote('"', name).to_string()
}
fn column(alias: &str, name: &str) -> String {
    format!("{}.{}", quoted(alias), quoted(name))
}
fn concat(parts: &[String]) -> String {
    if parts.is_empty() {
        EMPTY.into()
    } else {
        format!("list_concat({})", parts.join(", "))
    }
}
fn token(label: &str, identity: &str) -> String {
    let label = Expr::Value(Value::SingleQuotedString(label.into()));
    format!("list_value(struct_pack(t := {label}, r := {identity}))")
}

impl PassantRewriter {
    /// Stable registration IDs, including gaps left by deleted policies.
    pub fn registered_policy_indices(&self) -> Vec<usize> {
        self.store
            .policy_indices()
            .into_iter()
            .filter_map(|i| self.store.original_policy_index(i))
            .collect()
    }

    /// Rewrite a SELECT using the catalog's trusted intermediate channels.
    /// Generated channels are not revisited as user SQL or policy expressions.
    pub fn rewrite_intermediate_query(&self, sql: &str) -> Result<String, RewriteError> {
        let mut statements =
            Parser::parse_sql(&DuckDbDialect {}, sql).map_err(|e| error(&e.to_string()))?;
        if statements.len() != 1 {
            return Err(error("one SELECT statement is required"));
        }
        let Statement::Query(mut query) = statements.remove(0) else {
            return Err(error("one SELECT statement is required"));
        };
        // A generated window frame can repeat a parameter expression. Number
        // original positional parameters once so the clone keeps its binding.
        let mut index = 0;
        let _ = visit_expressions_mut(query.as_mut(), |expr| {
            if let Expr::Value(Value::Placeholder(name)) = expr
                && name == "?"
            {
                index += 1;
                *name = format!("${index}");
            }
            ControlFlow::<()>::Continue(())
        });
        let planner = Lineage {
            catalog: &self.catalog,
            registry: &self.aggregate_registry,
        };
        planner.query(&mut query, &HashMap::new(), false)?;
        Ok(query.to_string())
    }
}

struct Lineage<'a> {
    catalog: &'a TableCatalog,
    registry: &'a AggregateRegistry,
}
#[derive(Clone)]
struct Binding {
    alias: String,
    columns: Vec<String>,
    dependency: String,
}

impl Lineage<'_> {
    fn query(
        &self,
        query: &mut Query,
        inherited: &HashMap<String, Vec<String>>,
        named: bool,
    ) -> Result<Vec<String>, RewriteError> {
        if !query.locks.is_empty()
            || !query.limit_by.is_empty()
            || query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
        {
            return Err(error(
                "locking and nonrelational query clauses are unsupported",
            ));
        }
        for expr in query
            .limit
            .iter()
            .chain(query.offset.iter().map(|offset| &offset.value))
            .chain(
                query
                    .fetch
                    .iter()
                    .filter_map(|fetch| fetch.quantity.as_ref()),
            )
        {
            self.validate(expr, false)?;
        }
        let mut ctes = inherited.clone();
        if let Some(with) = &mut query.with {
            if with.recursive {
                return Err(error("recursive CTEs require a fixpoint state contract"));
            }
            for cte in &mut with.cte_tables {
                let explicit = !cte.alias.columns.is_empty();
                let mut columns = self.query(&mut cte.query, &ctes, !explicit)?;
                if explicit {
                    if cte.alias.columns.len() != columns.len() {
                        return Err(error(
                            "CTE column aliases must match the logical projection",
                        ));
                    }
                    columns = cte
                        .alias
                        .columns
                        .iter()
                        .map(|c| c.name.value.clone())
                        .collect();
                    cte.alias.columns.push(TableAliasColumnDef::from_name(DEPS));
                }
                if columns.iter().any(|c| c.eq_ignore_ascii_case(DEPS)) {
                    return Err(error("reserved intermediate metadata name"));
                }
                ctes.insert(cte.alias.name.value.to_ascii_lowercase(), columns);
            }
        }
        let SetExpr::Select(select) = query.body.as_mut() else {
            return Err(error(
                "set operations require a joint intermediate merge boundary",
            ));
        };
        if select.distinct.is_some()
            || select.qualify.is_some()
            || select.top.is_some()
            || select.into.is_some()
            || !select.named_window.is_empty()
            || !select.lateral_views.is_empty()
            || select.prewhere.is_some()
            || !select.cluster_by.is_empty()
            || !select.distribute_by.is_empty()
            || !select.sort_by.is_empty()
            || select.value_table_mode.is_some()
            || select.connect_by.is_some()
        {
            return Err(error(
                "DISTINCT, QUALIFY, named windows and nonrelational SELECT clauses are unsupported",
            ));
        }
        let mut bindings = Vec::new();
        for from in &mut select.from {
            bindings.push(self.factor(&mut from.relation, &ctes)?);
            for join in &mut from.joins {
                match &join.join_operator {
                    JoinOperator::Inner(JoinConstraint::On(condition)) => {
                        self.validate(condition, false)?
                    }
                    JoinOperator::CrossJoin | JoinOperator::Inner(JoinConstraint::None) => (),
                    _ => {
                        return Err(error(
                            "nullable, NATURAL, USING and non-inner joins need presence-aware lineage",
                        ));
                    }
                }
                bindings.push(self.factor(&mut join.relation, &ctes)?);
            }
        }
        let mut aliases = HashSet::new();
        if bindings
            .iter()
            .any(|b| !aliases.insert(b.alias.to_ascii_lowercase()))
        {
            return Err(error("ambiguous relation aliases"));
        }
        // Whole-row expressions can inspect hidden state even in predicates,
        // grouping and ordering; check each user expression in this scope only.
        let mut scoped = select
            .selection
            .iter()
            .chain(select.having.iter())
            .collect::<Vec<_>>();
        if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
            scoped.extend(exprs.iter());
        }
        if let Some(order) = &query.order_by {
            scoped.extend(order.exprs.iter().map(|item| &item.expr));
        }
        scoped.extend(query.limit.iter());
        scoped.extend(query.offset.iter().map(|offset| &offset.value));
        scoped.extend(
            query
                .fetch
                .iter()
                .filter_map(|fetch| fetch.quantity.as_ref()),
        );
        for from in &select.from {
            for join in &from.joins {
                if let JoinOperator::Inner(JoinConstraint::On(expr)) = &join.join_operator {
                    scoped.push(expr);
                }
            }
        }
        for expr in scoped {
            if visit_expressions(expr, |node| {
                if let Expr::Identifier(i) = node
                    && aliases.contains(&i.value.to_ascii_lowercase())
                {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            })
            .is_break()
            {
                return Err(error("whole-row relation references are unsupported"));
            }
        }
        let mut projections = Vec::new();
        for projection in std::mem::take(&mut select.projection) {
            let qualifier = match &projection {
                SelectItem::Wildcard(options) if *options == Default::default() => Some(None),
                SelectItem::QualifiedWildcard(name, options)
                    if *options == Default::default() && name.0.len() == 1 =>
                {
                    Some(Some(name.0[0].value.clone()))
                }
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    return Err(error("modified stars are unsupported"));
                }
                _ => None,
            };
            if let Some(qualifier) = qualifier {
                let matches: Vec<_> = bindings
                    .iter()
                    .filter(|b| {
                        qualifier
                            .as_ref()
                            .is_none_or(|q| q.eq_ignore_ascii_case(&b.alias))
                    })
                    .collect();
                if matches.is_empty() {
                    return Err(error("star has no relation binding"));
                }
                for binding in matches {
                    for name in &binding.columns {
                        projections.push(SelectItem::UnnamedExpr(parse_policy_expr_duckdb(
                            &column(&binding.alias, name),
                        )?));
                    }
                }
            } else {
                projections.push(projection);
            }
        }
        let mut output_columns = Vec::new();
        for projection in &projections {
            let (expr, name) = match projection {
                SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                SelectItem::UnnamedExpr(expr) => (
                    expr,
                    match expr {
                        Expr::Identifier(i) => Some(i.value.clone()),
                        Expr::CompoundIdentifier(ids) => ids.last().map(|i| i.value.clone()),
                        _ => None,
                    },
                ),
                _ => unreachable!(),
            };
            self.validate(expr, true)?;
            let alias_reference = visit_expressions(expr, |node| {
                if let Expr::Identifier(i) = node
                    && aliases.contains(&i.value.to_ascii_lowercase())
                {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            });
            if alias_reference.is_break() {
                return Err(error("whole-row relation references are unsupported"));
            }
            if named && name.is_none() {
                return Err(error(
                    "CTE/derived expressions require explicit output aliases",
                ));
            }
            output_columns.push(name.unwrap_or_else(|| expr.to_string()));
        }
        if let Some(order) = &query.order_by {
            for item in &order.exprs {
                self.validate(&item.expr, false)?;
                if let Expr::Value(Value::Number(n, _)) = &item.expr
                    && n.parse::<usize>().is_ok_and(|n| n > output_columns.len())
                {
                    return Err(error("ORDER BY position exceeds the logical projection"));
                }
            }
        }
        for expr in select.selection.iter().chain(select.having.iter()) {
            self.validate(expr, false)?;
        }
        let grouped = match &select.group_by {
            GroupByExpr::All(_) => return Err(error("GROUP BY ALL is unsupported")),
            GroupByExpr::Expressions(exprs, _) => {
                for expr in exprs {
                    self.validate(expr, false)?;
                }
                !exprs.is_empty()
            }
        };
        let mut windows = Vec::new();
        let mut aggregate = grouped
            || select
                .having
                .as_ref()
                .is_some_and(|expr| self.registry.expr_contains_aggregate(expr));
        let check = visit_expressions(&projections, |node| {
            if let Expr::Function(function) = node {
                if let Some(window) = &function.over {
                    let name = function.name.to_string().to_ascii_lowercase();
                    if name == "row_number" {
                        return ControlFlow::Continue(());
                    }
                    if !self
                        .registry
                        .lookup(&name)
                        .is_some_and(|a| a.source == AggregateFunctionSource::Builtin)
                        || function.filter.is_some()
                        || function.null_treatment.is_some()
                    {
                        return ControlFlow::Break(error(
                            "navigation/ranking, filtered and unknown windows need a distinct contributor contract",
                        ));
                    }
                    let WindowType::WindowSpec(spec) = window else {
                        return ControlFlow::Break(error("window specifications must be inline"));
                    };
                    if spec.window_name.is_some() {
                        return ControlFlow::Break(error("window specifications must be inline"));
                    }
                    if !windows.contains(window) {
                        windows.push(window.clone());
                    }
                } else if self.registry.is_aggregate_call(function) {
                    aggregate = true;
                }
            }
            ControlFlow::Continue(())
        });
        if let ControlFlow::Break(e) = check {
            return Err(e);
        }
        if aggregate && !windows.is_empty() {
            return Err(error(
                "factor grouped window inputs into a CTE before windowing",
            ));
        }
        let base = concat(
            &bindings
                .iter()
                .map(|b| b.dependency.clone())
                .collect::<Vec<_>>(),
        );
        let dependency = if aggregate {
            format!("COALESCE(list_distinct(flatten(list({base}))), {EMPTY})")
        } else if windows.is_empty() {
            base
        } else {
            let mut dependencies = vec![base.clone()];
            for window in windows {
                dependencies.push(format!(
                    "COALESCE(flatten(list({base}) OVER {window}), {EMPTY})"
                ));
            }
            format!("list_distinct({})", concat(&dependencies))
        };
        projections.push(SelectItem::ExprWithAlias {
            expr: parse_policy_expr_duckdb(&dependency)?,
            alias: Ident::new(DEPS),
        });
        select.projection = projections;
        Ok(output_columns)
    }

    fn validate(&self, expression: &Expr, window: bool) -> Result<(), RewriteError> {
        let result = visit_expressions(expression, |node| {
            match node {
                Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                    return ControlFlow::Break(error(
                        "expression subqueries require their own lineage boundary",
                    ));
                }
                Expr::Identifier(i) if i.value.starts_with("_dfc_intermediate_") => {
                    return ControlFlow::Break(error("reserved intermediate metadata name"));
                }
                Expr::CompoundIdentifier(ids)
                    if ids
                        .iter()
                        .any(|i| i.value.starts_with("_dfc_intermediate_")) =>
                {
                    return ControlFlow::Break(error("reserved intermediate metadata name"));
                }
                Expr::Function(f)
                    if matches!(f.args, FunctionArguments::Subquery(_))
                        || matches!(f.parameters, FunctionArguments::Subquery(_)) =>
                {
                    return ControlFlow::Break(error(
                        "expression subqueries require their own lineage boundary",
                    ));
                }
                Expr::Function(f) if f.name.to_string().eq_ignore_ascii_case("columns") => {
                    return ControlFlow::Break(error(
                        "dynamic COLUMNS projections are unsupported",
                    ));
                }
                Expr::Function(f) if f.over.is_some() && !window => {
                    return ControlFlow::Break(error(
                        "windows outside the projection are unsupported",
                    ));
                }
                _ => (),
            }
            ControlFlow::Continue(())
        });
        match result {
            ControlFlow::Break(e) => Err(e),
            _ => Ok(()),
        }
    }

    fn factor(
        &self,
        factor: &mut TableFactor,
        ctes: &HashMap<String, Vec<String>>,
    ) -> Result<Binding, RewriteError> {
        match factor {
            TableFactor::Table {
                name,
                alias,
                args,
                version,
                ..
            } if args.is_none() && version.is_none() && name.0.len() == 1 => {
                if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                    return Err(error("relation column alias lists are unsupported"));
                }
                let key = name.0[0].value.to_ascii_lowercase();
                let alias = alias
                    .as_ref()
                    .map_or_else(|| name.0[0].value.clone(), |a| a.name.value.clone());
                if let Some(columns) = ctes.get(&key) {
                    return Ok(Binding {
                        alias: alias.clone(),
                        columns: columns.clone(),
                        dependency: column(&alias, DEPS),
                    });
                }
                let columns = self
                    .catalog
                    .columns(&name.to_string())
                    .ok_or_else(|| error("missing catalog relation"))?
                    .to_vec();
                let mut dependencies = Vec::new();
                if let Some(binding) = self.catalog.intermediate_binding(&name.to_string()) {
                    if let Some(source) = &binding.source {
                        dependencies.push(token(
                            &format!("source:{}", source.to_ascii_lowercase()),
                            &column(&alias, "rowid"),
                        ));
                    }
                    if let Some(state_column) = &binding.state_column {
                        dependencies.push(token("state", &column(&alias, state_column)));
                    }
                }
                Ok(Binding {
                    alias,
                    columns,
                    dependency: concat(&dependencies),
                })
            }
            TableFactor::Derived {
                lateral: false,
                subquery,
                alias: Some(alias),
            } => {
                if !alias.columns.is_empty() {
                    return Err(error("derived column alias lists are unsupported"));
                }
                let columns = self.query(subquery, ctes, true)?;
                Ok(Binding {
                    alias: alias.name.value.clone(),
                    columns,
                    dependency: column(&alias.name.value, DEPS),
                })
            }
            _ => Err(error(
                "only physical tables, CTEs and aliased non-lateral derived tables are supported",
            )),
        }
    }
}
