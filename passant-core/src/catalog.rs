//! Adapter catalog snapshots and policy validation at registration time.
//!
//! `TableCatalog` holds normalized table/column facts from Python adapters.
//! Validation is split between constraint syntax (`constraint_syntax`) and
//! catalog membership (`catalog_validation`).

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use sqlparser::ast::{Expr, Function, FunctionArguments, ObjectName};

use crate::diagnostics::{ErrorKind, RewriteError};
use crate::identifiers::{ColumnName, QualifiedColumn, SinkName, SourceName, TableKey};
use crate::policy::{PolicyIr, Resolution};
use crate::sql::parse_projection_expr;

/// Catalog facts supplied at policy registration time (from any adapter snapshot).
#[derive(Debug, Default, Clone)]
pub struct TableCatalog {
    table_columns: HashMap<TableKey, Vec<String>>,
    column_types: HashMap<(TableKey, String), String>,
    unique_columns: HashSet<(TableKey, String)>,
    table_row_counts: HashMap<TableKey, u64>,
    loaded: bool,
}

/// JSON catalog snapshot from Python adapter introspection.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CatalogSnapshot {
    #[serde(default)]
    pub dialect: Option<String>,
    #[serde(default)]
    pub default_schema: Option<String>,
    #[serde(default)]
    pub search_path: Vec<String>,
    #[serde(default)]
    pub tables: HashMap<String, CatalogTableInfo>,
    #[serde(default)]
    pub unique_columns: Vec<[String; 2]>,
}

impl CatalogSnapshot {
    pub fn sql_dialect(&self) -> crate::sql::SqlDialect {
        self.dialect
            .as_deref()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogTableInfo {
    pub columns: Vec<String>,
    #[serde(default)]
    pub types: HashMap<String, String>,
    /// Optional row count from adapter introspection (used for singleton DIMENSION joins).
    #[serde(default)]
    pub row_count: Option<u64>,
}

impl TableCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_snapshot(snapshot: CatalogSnapshot) -> Self {
        let mut catalog = Self::new();
        for (table, info) in snapshot.tables {
            catalog.register_table(table.clone(), info.columns.clone());
            for (column, sql_type) in info.types {
                catalog.register_column_type(table.clone(), column, sql_type);
            }
            if let Some(row_count) = info.row_count {
                catalog.register_table_row_count(table.clone(), row_count);
            }
        }
        for unique in snapshot.unique_columns {
            if unique.len() == 2 {
                catalog.register_unique_column(unique[0].clone(), unique[1].clone());
            }
        }
        catalog.loaded = true;
        catalog
    }

    pub fn load_snapshot(&mut self, snapshot: CatalogSnapshot) {
        *self = Self::from_snapshot(snapshot);
        self.loaded = true;
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn register_table(&mut self, table: impl Into<String>, columns: Vec<String>) {
        self.table_columns
            .insert(TableKey::new(&table.into()), columns);
    }

    pub fn register_column_type(
        &mut self,
        table: impl Into<String>,
        column: impl Into<String>,
        sql_type: impl Into<String>,
    ) {
        self.column_types.insert(
            (
                TableKey::new(&table.into()),
                ColumnName::new(column.into()).key(),
            ),
            sql_type.into().to_ascii_uppercase(),
        );
    }

    pub fn register_unique_column(&mut self, table: impl Into<String>, column: impl Into<String>) {
        self.unique_columns.insert((
            TableKey::new(&table.into()),
            ColumnName::new(column.into()).key(),
        ));
    }

    pub fn register_table_row_count(&mut self, table: impl Into<String>, row_count: u64) {
        self.table_row_counts
            .insert(TableKey::new(&table.into()), row_count);
    }

    pub fn table_row_count(&self, table: &str) -> Option<u64> {
        self.table_row_counts.get(&TableKey::new(table)).copied()
    }

    pub fn is_singleton_table(&self, table: &str) -> bool {
        self.table_row_count(table) == Some(1)
    }

    pub fn table_exists(&self, table: &str) -> bool {
        self.table_columns.contains_key(&TableKey::new(table))
    }

    pub fn columns(&self, table: &str) -> Option<&[String]> {
        self.table_columns
            .get(&TableKey::new(table))
            .map(Vec::as_slice)
    }

    pub fn column_type(&self, table: &str, column: &str) -> Option<&str> {
        self.column_types
            .get(&(TableKey::new(table), ColumnName::new(column).key()))
            .map(String::as_str)
    }

    pub fn is_unique_column(&self, table: &str, column: &str) -> bool {
        self.unique_columns
            .contains(&(TableKey::new(table), ColumnName::new(column).key()))
    }

    pub fn validate_policy(&self, policy: &PolicyIr) -> Result<(), RewriteError> {
        if !self.loaded {
            return Ok(());
        }
        let PolicyIr::Pgn {
            sources,
            dimension_tables,
            dimension_aliases,
            dimension_queries,
            sink,
            sink_alias,
            source_aliases,
            constraint,
            on_fail,
            ..
        } = policy;
        validate_pgn_policy(
            self,
            sources,
            dimension_tables,
            dimension_aliases,
            dimension_queries,
            sink.as_deref(),
            sink_alias.as_deref(),
            source_aliases,
            constraint,
            on_fail.clone(),
        )
    }
}

/// Syntax validation for policy constraint and dimension expressions.
///
/// Parses the expression as SQL and requires all column references to be qualified.
pub fn validate_constraint_expression(sql: &str, label: &str) -> Result<(), RewriteError> {
    let sql = crate::rewriter::preprocess_policy_constraint(sql);
    validate_qualified_columns(&sql, label)
}

#[allow(clippy::too_many_arguments)]
fn validate_pgn_policy(
    catalog: &TableCatalog,
    sources: &[String],
    dimension_tables: &[String],
    dimension_aliases: &HashMap<String, String>,
    dimension_queries: &HashMap<String, String>,
    sink: Option<&str>,
    sink_alias: Option<&str>,
    source_aliases: &HashMap<String, String>,
    constraint: &str,
    _on_fail: Resolution,
) -> Result<(), RewriteError> {
    let constraint = crate::rewriter::preprocess_policy_constraint(constraint);
    let mut source_columns = HashMap::new();
    for source in sources {
        let source_name = SourceName::parse(source);
        if !catalog.table_exists(source_name.as_str()) {
            return Err(RewriteError::catalog_with_context(
                ErrorKind::UnknownTable,
                format!("Source table '{source}' does not exist"),
                Some(source.clone()),
                None,
                Some(constraint.to_string()),
                Some("catalog_validation"),
            ));
        }
        let table_key = TableKey::from_source(&source_name);
        source_columns.insert(
            table_key.clone(),
            catalog
                .columns(source_name.as_str())
                .unwrap_or_default()
                .iter()
                .map(|column| ColumnName::new(column).key())
                .collect::<HashSet<_>>(),
        );
    }

    let sink_columns = if let Some(sink) = sink {
        let sink_name = SinkName::parse(sink);
        if !catalog.table_exists(sink_name.as_str()) {
            return Err(RewriteError::catalog_with_context(
                ErrorKind::UnknownTable,
                format!("Sink table '{sink}' does not exist"),
                Some(sink.to_string()),
                None,
                Some(constraint.to_string()),
                Some("catalog_validation"),
            ));
        }
        Some(
            catalog
                .columns(sink_name.as_str())
                .unwrap_or_default()
                .iter()
                .map(|column| ColumnName::new(column).key())
                .collect::<HashSet<_>>(),
        )
    } else {
        None
    };

    validate_qualified_columns(&constraint, "constraint")?;

    let mut dimension_columns = HashMap::new();
    for table in dimension_tables {
        if !catalog.table_exists(table) {
            return Err(RewriteError::catalog_with_context(
                ErrorKind::UnknownTable,
                format!("Dimension table '{table}' does not exist"),
                Some(table.clone()),
                None,
                Some(constraint.to_string()),
                Some("catalog_validation"),
            ));
        }
        let columns = catalog
            .columns(table)
            .unwrap_or_default()
            .iter()
            .map(|column| ColumnName::new(column).key())
            .collect::<HashSet<_>>();
        dimension_columns.insert(TableKey::new(table), columns);
    }
    for (alias, base) in dimension_aliases {
        if dimension_queries.contains_key(alias) {
            continue;
        }
        if !catalog.table_exists(base) {
            return Err(RewriteError::catalog_with_context(
                ErrorKind::UnknownTable,
                format!("Dimension table '{base}' does not exist"),
                Some(base.clone()),
                None,
                Some(constraint.to_string()),
                Some("catalog_validation"),
            ));
        }
        let columns = catalog
            .columns(base)
            .unwrap_or_default()
            .iter()
            .map(|column| ColumnName::new(column).key())
            .collect::<HashSet<_>>();
        dimension_columns.insert(TableKey::new(alias), columns.clone());
        dimension_columns.insert(TableKey::new(base), columns);
    }

    let source_names = sources
        .iter()
        .map(|source| TableKey::new(source))
        .collect::<HashSet<_>>();
    let mut source_qualifier_names = source_names.clone();
    for alias in source_aliases.keys() {
        source_qualifier_names.insert(TableKey::new(alias));
    }
    let sink_overlaps_source = sink.is_some_and(|sink_name| {
        sources
            .iter()
            .any(|source| source.eq_ignore_ascii_case(sink_name))
    });
    let mut sink_names = HashSet::new();
    if let Some(sink) = sink
        && !(sink_overlaps_source && sink_alias.is_some())
    {
        sink_names.insert(TableKey::new(sink));
    }
    sink_names.insert(TableKey::new("_output_"));
    if let Some(sink_alias) = sink_alias {
        sink_names.insert(TableKey::new(sink_alias));
    }

    if !sources.is_empty() && sink.is_none() {
        let sink_equality_sources =
            source_columns_in_sink_equality(&constraint, &source_qualifier_names, &sink_names);
        let implicit_uniqueness_columns =
            implicit_uniqueness_source_columns(&constraint, &source_qualifier_names);
        let unaggregated =
            unaggregated_source_columns(&constraint, &source_qualifier_names, source_aliases)?
                .into_iter()
                .filter(|qualified| !is_catalog_unique_source_column(catalog, qualified))
                .filter(|qualified| {
                    !implicit_uniqueness_columns.contains(&qualified.to_ascii_lowercase())
                })
                .filter(|qualified| {
                    !sink_equality_sources.contains(&qualified.to_ascii_lowercase())
                })
                .collect::<Vec<_>>();
        if !unaggregated.is_empty() {
            return Err(RewriteError::catalog_with_context(
                ErrorKind::UnaggregatedSourceColumn,
                format!(
                    "Source columns in Policy constraints must be aggregated: {}",
                    unaggregated.join(", ")
                ),
                None,
                None,
                Some(constraint.to_string()),
                Some("catalog_validation"),
            ));
        }
    } else if !sources.is_empty() {
        let _ = source_columns_in_sink_equality(&constraint, &source_qualifier_names, &sink_names);
    }

    let referenced_columns = qualified_columns(&constraint)?;

    for column in referenced_columns {
        let table_name = column.table.as_str();
        let column_name = column.column.as_str();
        let table_key = TableKey::from_table(&column.table);
        let column_key = column.column.key();
        if let Some(base_source_key) =
            resolve_source_table_key(&table_key, &source_names, source_aliases)
        {
            let Some(columns) = source_columns.get(&base_source_key) else {
                continue;
            };
            if !columns.contains(&column_key) {
                let base_name = base_source_key.as_str();
                return Err(RewriteError::catalog_with_context(
                    ErrorKind::UnknownColumn,
                    format!(
                        "Column '{table_name}.{column_name}' referenced in constraint \
                         does not exist in source table '{base_name}'"
                    ),
                    Some(table_name.to_string()),
                    Some(column_name.to_string()),
                    Some(constraint.to_string()),
                    Some("catalog_validation"),
                ));
            }
        } else if sink_names.contains(&table_key) {
            if let Some(columns) = &sink_columns
                && !columns.contains(&column_key)
            {
                return Err(RewriteError::catalog_with_context(
                    ErrorKind::UnknownColumn,
                    format!(
                        "Column '{table_name}.{column_name}' referenced in constraint \
                         does not exist in sink table '{}'",
                        sink.unwrap_or(table_name)
                    ),
                    Some(table_name.to_string()),
                    Some(column_name.to_string()),
                    Some(constraint.to_string()),
                    Some("catalog_validation"),
                ));
            }
        } else if let Some(columns) = resolve_dimension_columns(
            &table_key,
            dimension_tables,
            dimension_aliases,
            &dimension_columns,
        ) {
            if !columns.contains(&column_key) {
                let base = dimension_aliases
                    .get(table_key.as_str())
                    .map(String::as_str)
                    .unwrap_or(table_name);
                return Err(RewriteError::catalog_with_context(
                    ErrorKind::UnknownColumn,
                    format!(
                        "Column '{table_name}.{column_name}' referenced in constraint \
                         does not exist in dimension table '{base}'"
                    ),
                    Some(table_name.to_string()),
                    Some(column_name.to_string()),
                    Some(constraint.to_string()),
                    Some("catalog_validation"),
                ));
            }
        } else {
            return Err(RewriteError::catalog_with_context(
                ErrorKind::UnknownColumn,
                format!(
                    "Column '{table_name}.{column_name}' referenced in constraint \
                     references table '{table_name}', which is not in sources ({sources:?}) \
                     or sink ('{sink:?}')"
                ),
                Some(table_name.to_string()),
                Some(column_name.to_string()),
                Some(constraint.to_string()),
                Some("catalog_validation"),
            ));
        }
    }

    Ok(())
}

fn parse_constraint_expr(sql: &str) -> Result<Expr, RewriteError> {
    parse_projection_expr(sql).map_err(|_| {
        RewriteError::unsupported_statement(format!("Invalid constraint SQL expression '{sql}'"))
    })
}

fn validate_qualified_columns(sql: &str, label: &str) -> Result<(), RewriteError> {
    let expr = parse_constraint_expr(sql)?;
    let unqualified = UnqualifiedColumnCollector::collect(&expr);
    if !unqualified.is_empty() {
        return Err(RewriteError::catalog_with_context(
            ErrorKind::UnqualifiedColumn,
            format!(
                "All columns in constraints and dimensions must be qualified with table names. \
                 Unqualified columns found: {}",
                unqualified.join(", ")
            ),
            None,
            None,
            Some(sql.to_string()),
            Some("constraint_syntax"),
        ));
    }
    let _ = label;
    Ok(())
}

fn qualified_columns(sql: &str) -> Result<Vec<QualifiedColumn>, RewriteError> {
    let expr = parse_constraint_expr(sql)?;
    Ok(crate::sql::collect_qualified_columns_from_expr(&expr))
}

fn is_catalog_unique_source_column(catalog: &TableCatalog, qualified: &str) -> bool {
    let Some((table, column)) = qualified.rsplit_once('.') else {
        return false;
    };
    catalog.is_unique_column(table, column)
}

fn resolve_dimension_columns<'a>(
    table_key: &TableKey,
    dimension_tables: &[String],
    dimension_aliases: &HashMap<String, String>,
    dimension_columns: &'a HashMap<TableKey, HashSet<String>>,
) -> Option<&'a HashSet<String>> {
    if dimension_tables
        .iter()
        .any(|table| TableKey::new(table) == *table_key)
    {
        return dimension_columns.get(table_key);
    }
    if let Some(base) = dimension_aliases.get(table_key.as_str()) {
        return dimension_columns.get(&TableKey::new(base));
    }
    dimension_columns.get(table_key)
}

fn resolve_source_table_key(
    table_key: &TableKey,
    source_names: &HashSet<TableKey>,
    source_aliases: &HashMap<String, String>,
) -> Option<TableKey> {
    if source_names.contains(table_key) {
        return Some(table_key.clone());
    }
    source_aliases
        .get(table_key.as_str())
        .map(|base| TableKey::new(base))
        .filter(|base| source_names.contains(base))
}

fn source_columns_in_sink_equality(
    sql: &str,
    source_qualifier_names: &HashSet<TableKey>,
    sink_names: &HashSet<TableKey>,
) -> HashSet<String> {
    let Ok(expr) = parse_constraint_expr(sql) else {
        return HashSet::new();
    };
    let mut found = HashSet::new();
    collect_source_sink_equality_pairs(&expr, source_qualifier_names, sink_names, &mut found);
    found
}

fn collect_source_sink_equality_pairs(
    expr: &Expr,
    source_qualifier_names: &HashSet<TableKey>,
    sink_names: &HashSet<TableKey>,
    found: &mut HashSet<String>,
) {
    match expr {
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::Eq | sqlparser::ast::BinaryOperator::NotEq,
            right,
        } => {
            if let (Some(source), Some(sink)) = (
                qualified_source_column(left, source_qualifier_names),
                qualified_sink_column(right, sink_names),
            ) {
                found.insert(source.to_ascii_lowercase());
                let _ = sink;
            } else if let (Some(source), Some(sink)) = (
                qualified_source_column(right, source_qualifier_names),
                qualified_sink_column(left, sink_names),
            ) {
                found.insert(source.to_ascii_lowercase());
                let _ = sink;
            }
            collect_source_sink_equality_pairs(left, source_qualifier_names, sink_names, found);
            collect_source_sink_equality_pairs(right, source_qualifier_names, sink_names, found);
        }
        Expr::Nested(inner) => {
            collect_source_sink_equality_pairs(inner, source_qualifier_names, sink_names, found);
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_source_sink_equality_pairs(left, source_qualifier_names, sink_names, found);
            collect_source_sink_equality_pairs(right, source_qualifier_names, sink_names, found);
        }
        _ => {}
    }
}

fn qualified_source_column(
    expr: &Expr,
    source_qualifier_names: &HashSet<TableKey>,
) -> Option<String> {
    let column = QualifiedColumn::from_expr(expr)?;
    if source_qualifier_names.contains(&TableKey::from_table(&column.table)) {
        Some(column.display_sql())
    } else {
        None
    }
}

fn qualified_sink_column(expr: &Expr, sink_names: &HashSet<TableKey>) -> Option<String> {
    let column = QualifiedColumn::from_expr(expr)?;
    if sink_names.contains(&TableKey::from_table(&column.table)) {
        Some(column.display_sql())
    } else {
        None
    }
}

fn implicit_uniqueness_source_columns(
    sql: &str,
    source_qualifier_names: &HashSet<TableKey>,
) -> HashSet<String> {
    let Ok(expr) = parse_constraint_expr(sql) else {
        return HashSet::new();
    };
    let mut found = HashSet::new();
    collect_implicit_uniqueness_source_columns(&expr, source_qualifier_names, &mut found);
    found
}

fn collect_implicit_uniqueness_source_columns(
    expr: &Expr,
    source_qualifier_names: &HashSet<TableKey>,
    found: &mut HashSet<String>,
) {
    if let Some(column) = column_value_comparison_source(expr, source_qualifier_names) {
        found.insert(column);
    }
    match expr {
        Expr::BinaryOp { left, right, .. } => {
            collect_implicit_uniqueness_source_columns(left, source_qualifier_names, found);
            collect_implicit_uniqueness_source_columns(right, source_qualifier_names, found);
        }
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => {
            collect_implicit_uniqueness_source_columns(inner, source_qualifier_names, found);
        }
        _ => {}
    }
}

fn column_value_comparison_source(
    expr: &Expr,
    source_qualifier_names: &HashSet<TableKey>,
) -> Option<String> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    if !matches!(
        op,
        sqlparser::ast::BinaryOperator::Eq
            | sqlparser::ast::BinaryOperator::NotEq
            | sqlparser::ast::BinaryOperator::Gt
            | sqlparser::ast::BinaryOperator::Lt
            | sqlparser::ast::BinaryOperator::GtEq
            | sqlparser::ast::BinaryOperator::LtEq
    ) {
        return None;
    }
    if let Some(column) = QualifiedColumn::from_expr(left) {
        if QualifiedColumn::from_expr(right).is_some() {
            return None;
        }
        if source_qualifier_names.contains(&TableKey::from_table(&column.table)) {
            return Some(column.display_sql().to_ascii_lowercase());
        }
    }
    if let Some(column) = QualifiedColumn::from_expr(right) {
        if QualifiedColumn::from_expr(left).is_some() {
            return None;
        }
        if source_qualifier_names.contains(&TableKey::from_table(&column.table)) {
            return Some(column.display_sql().to_ascii_lowercase());
        }
    }
    None
}

fn unaggregated_source_columns(
    sql: &str,
    source_qualifier_names: &HashSet<TableKey>,
    source_aliases: &HashMap<String, String>,
) -> Result<Vec<String>, RewriteError> {
    let expr = parse_constraint_expr(sql)?;
    Ok(UnaggregatedSourceColumnCollector::collect(
        &expr,
        source_qualifier_names,
        source_aliases,
    ))
}

struct UnqualifiedColumnCollector {
    found: Vec<String>,
}

impl UnqualifiedColumnCollector {
    fn collect(expr: &Expr) -> Vec<String> {
        let mut collector = Self { found: Vec::new() };
        collector.visit(expr, false);
        collector.found
    }

    fn visit(&mut self, expr: &Expr, inside_aggregate: bool) {
        if let Expr::Identifier(ident) = expr {
            self.found.push(ident.value.clone());
        }
        expr_visit_children(
            expr,
            |child, inside| self.visit(child, inside),
            inside_aggregate,
        );
    }
}

struct UnaggregatedSourceColumnCollector {
    source_qualifier_names: HashSet<TableKey>,
    found: Vec<String>,
    seen: HashSet<String>,
}

impl UnaggregatedSourceColumnCollector {
    fn collect(
        expr: &Expr,
        source_qualifier_names: &HashSet<TableKey>,
        _source_aliases: &HashMap<String, String>,
    ) -> Vec<String> {
        let mut collector = Self {
            source_qualifier_names: source_qualifier_names.clone(),
            found: Vec::new(),
            seen: HashSet::new(),
        };
        collector.visit_expr(expr);
        collector.found
    }

    fn visit_expr(&mut self, expr: &Expr) {
        self.visit_expr_with_context(expr, false);
    }

    fn visit_expr_with_context(&mut self, expr: &Expr, inside_aggregate: bool) {
        let next_inside = inside_aggregate || expr_is_aggregate(expr);
        if let Some(column) = QualifiedColumn::from_expr(expr)
            && !next_inside
            && self
                .source_qualifier_names
                .contains(&TableKey::from_table(&column.table))
        {
            let qualified = column.display_sql();
            let key = qualified.to_ascii_lowercase();
            if self.seen.insert(key) {
                self.found.push(qualified);
            }
        }
        expr_visit_children(
            expr,
            |child, inside| {
                self.visit_expr_with_context(child, inside);
            },
            next_inside,
        );
    }
}

fn expr_visit_children(expr: &Expr, mut visit: impl FnMut(&Expr, bool), inside_aggregate: bool) {
    match expr {
        Expr::UnaryOp { expr, .. } => visit(expr, inside_aggregate),
        Expr::BinaryOp { left, right, .. } => {
            visit(left, inside_aggregate);
            visit(right, inside_aggregate);
        }
        Expr::Nested(inner) => visit(inner, inside_aggregate),
        Expr::Function(Function { args, .. }) => {
            let inside = true;
            match args {
                FunctionArguments::None => {}
                FunctionArguments::Subquery(_) => {}
                FunctionArguments::List(list) => {
                    for arg in &list.args {
                        if let sqlparser::ast::FunctionArg::Unnamed(
                            sqlparser::ast::FunctionArgExpr::Expr(inner),
                        ) = arg
                        {
                            visit(inner, inside);
                        }
                    }
                }
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                visit(operand, inside_aggregate);
            }
            for (condition, result) in conditions.iter().zip(results.iter()) {
                visit(condition, inside_aggregate);
                visit(result, inside_aggregate);
            }
            if let Some(else_result) = else_result {
                visit(else_result, inside_aggregate);
            }
        }
        Expr::InSubquery { expr, .. } | Expr::InList { expr, .. } => visit(expr, inside_aggregate),
        Expr::Between {
            expr, low, high, ..
        } => {
            visit(expr, inside_aggregate);
            visit(low, inside_aggregate);
            visit(high, inside_aggregate);
        }
        Expr::IsNull(expr) | Expr::IsNotNull(expr) => visit(expr, inside_aggregate),
        Expr::Cast { expr, .. } => visit(expr, inside_aggregate),
        _ => {}
    }
}

fn expr_is_aggregate(expr: &Expr) -> bool {
    matches!(expr, Expr::Function(function) if is_aggregate_name(&function_name(function)))
}

fn function_name(function: &Function) -> String {
    match &function.name {
        ObjectName(parts) => parts
            .iter()
            .map(|part| part.value.clone())
            .collect::<Vec<_>>()
            .join("."),
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sum"
            | "count"
            | "avg"
            | "min"
            | "max"
            | "array_agg"
            | "string_agg"
            | "count_if"
            | "countif"
            | "list"
            | "any_value"
            | "bool_and"
            | "bool_or"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyIr;

    fn sample_catalog() -> TableCatalog {
        let mut catalog = TableCatalog::new();
        catalog.register_table("foo", vec!["id".into(), "region".into()]);
        catalog.register_table("reports", vec!["id".into(), "active".into()]);
        catalog.register_column_type("reports", "active", "BOOLEAN");
        catalog.loaded = true;
        catalog
    }

    #[test]
    fn table_exists_matches_schema_qualified_names() {
        let mut catalog = TableCatalog::new();
        catalog.register_table("MySchema.MyTable", vec!["id".into()]);
        catalog.loaded = true;
        assert!(catalog.table_exists("myschema.mytable"));
        assert!(catalog.table_exists("\"MySchema\".\"MyTable\""));
    }

    #[test]
    fn rejects_unqualified_constraint_column() {
        let err = validate_constraint_expression("max(id) > 1", "constraint").expect_err("reject");
        assert_eq!(err.kind(), ErrorKind::UnqualifiedColumn);
        if let RewriteError::Catalog(details) = err {
            assert_eq!(details.constraint.as_deref(), Some("max(id) > 1"));
            assert_eq!(
                details.validation_phase.as_deref(),
                Some("constraint_syntax")
            );
        } else {
            panic!("expected catalog validation error");
        }
    }

    #[test]
    fn rejects_missing_source_table() {
        let catalog = sample_catalog();
        let policy = PolicyIr::Pgn {
            sources: vec!["missing".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "max(missing.id) > 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog.validate_policy(&policy).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownTable);
    }

    #[test]
    fn rejects_unaggregated_source_column() {
        let catalog = sample_catalog();
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "foo.id IS NOT NULL".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog.validate_policy(&policy).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnaggregatedSourceColumn);
    }

    #[test]
    fn rejects_missing_source_column() {
        let catalog = sample_catalog();
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "max(foo.missing) > 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog.validate_policy(&policy).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownColumn);
    }

    #[test]
    fn validates_constraint_columns_through_dimension_alias() {
        let mut catalog = sample_catalog();
        catalog.register_table("catalog_users", vec!["id".into(), "name".into()]);
        let mut dimension_aliases = HashMap::new();
        dimension_aliases.insert("u".to_string(), "catalog_users".to_string());
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: vec!["catalog_users".into()],
            dimension_aliases,
            dimension_queries: HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "max(foo.id) > 1 AND u.id = 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        catalog
            .validate_policy(&policy)
            .expect("dimension alias column should validate");
    }

    #[test]
    fn rejects_unknown_dimension_column() {
        let mut catalog = sample_catalog();
        catalog.register_table("regions", vec!["id".into(), "code".into()]);
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: vec!["regions".into()],
            dimension_aliases: HashMap::new(),
            dimension_queries: HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "max(foo.id) > 1 AND regions.missing = 'x'".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog.validate_policy(&policy).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownColumn);
    }

    #[test]
    fn validates_constraint_columns_through_source_alias() {
        let catalog = sample_catalog();
        let mut source_aliases = HashMap::new();
        source_aliases.insert("f".to_string(), "foo".to_string());
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases,
            constraint: "max(f.id) > 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        catalog
            .validate_policy(&policy)
            .expect("alias-qualified source column should validate");
    }
}
