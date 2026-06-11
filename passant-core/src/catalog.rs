//! Adapter catalog snapshots and policy validation at registration time.
//!
//! `TableCatalog` holds normalized table/column facts from Python adapters.
//! Validation is split between constraint syntax (`constraint_syntax`) and
//! catalog membership (`catalog_validation`).

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use sqlparser::ast::{Expr, FunctionArguments};

use crate::aggregate_registry::{AggregateRegistry, function_name, normalize_name};
use crate::diagnostics::{ErrorKind, RewriteError};
use crate::identifiers::{ColumnName, QualifiedColumn, SinkName, SourceName, TableKey};
use crate::policy::{PolicyIr, Resolution};
use crate::policy_compile::{
    ParsedPolicyConstraint, parse_policy_constraint, parse_policy_constraint_with_registry,
};
use crate::sql::collect_query_projection_column_names;

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
    #[serde(default)]
    pub aggregate_functions: Vec<crate::aggregate_registry::AggregateFunctionSnapshot>,
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

    /// Column names registered as unique/primary-key for `table` (for UI edited UPDATE identity).
    pub fn unique_column_names(&self, table: &str) -> Vec<String> {
        let key = TableKey::new(table);
        let mut columns: Vec<String> = self
            .unique_columns
            .iter()
            .filter(|(table_key, _)| table_key == &key)
            .map(|(_, column_key)| column_key.as_str().to_string())
            .collect();
        columns.sort();
        columns.dedup();
        columns
    }

    pub fn validate_policy(
        &self,
        policy: &PolicyIr,
        registry: &AggregateRegistry,
    ) -> Result<(), RewriteError> {
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
        let parsed = parse_policy_constraint_with_registry(constraint, registry)?;
        validate_pgn_policy_parsed(
            self,
            registry,
            sources,
            dimension_tables,
            dimension_aliases,
            dimension_queries,
            sink.as_deref(),
            sink_alias.as_deref(),
            source_aliases,
            on_fail.clone(),
            &parsed,
        )
    }

    pub(crate) fn validate_pgn_policy_parsed(
        &self,
        policy: &PolicyIr,
        parsed: &ParsedPolicyConstraint,
        registry: &AggregateRegistry,
    ) -> Result<(), RewriteError> {
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
            on_fail,
            ..
        } = policy;
        validate_pgn_policy_parsed(
            self,
            registry,
            sources,
            dimension_tables,
            dimension_aliases,
            dimension_queries,
            sink.as_deref(),
            sink_alias.as_deref(),
            source_aliases,
            on_fail.clone(),
            parsed,
        )
    }
}

/// Syntax validation for policy constraint and dimension expressions.
///
/// Parses the expression as SQL and requires all column references to be qualified.
pub fn validate_constraint_expression(sql: &str, label: &str) -> Result<(), RewriteError> {
    let parsed = parse_policy_constraint(sql)?;
    if parsed.unqualified_columns.is_empty() {
        return Ok(());
    }
    let _ = label;
    Err(RewriteError::catalog_with_context(
        ErrorKind::UnqualifiedColumn,
        format!(
            "All columns in constraints and dimensions must be qualified with table names. \
             Unqualified columns found: {}",
            parsed.unqualified_columns.join(", ")
        ),
        None,
        None,
        Some(parsed.sql),
        Some("constraint_syntax"),
    ))
}

#[allow(clippy::too_many_arguments)]
fn validate_pgn_policy_parsed(
    catalog: &TableCatalog,
    registry: &AggregateRegistry,
    sources: &[String],
    dimension_tables: &[String],
    dimension_aliases: &HashMap<String, String>,
    dimension_queries: &HashMap<String, String>,
    sink: Option<&str>,
    sink_alias: Option<&str>,
    source_aliases: &HashMap<String, String>,
    _on_fail: Resolution,
    parsed: &ParsedPolicyConstraint,
) -> Result<(), RewriteError> {
    if !parsed.unqualified_columns.is_empty() {
        return Err(RewriteError::catalog_with_context(
            ErrorKind::UnqualifiedColumn,
            format!(
                "All columns in constraints and dimensions must be qualified with table names. \
                 Unqualified columns found: {}",
                parsed.unqualified_columns.join(", ")
            ),
            None,
            None,
            Some(parsed.sql.clone()),
            Some("catalog_validation"),
        ));
    }
    let constraint = parsed.sql.clone();
    let expr = &parsed.expr;
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
    for (alias, query_sql) in dimension_queries {
        let columns = collect_query_projection_column_names(query_sql)
            .map_err(|err| {
                RewriteError::catalog_with_context(
                    ErrorKind::UnknownColumn,
                    format!(
                        "Dimension subquery '{alias}' has invalid projection for catalog validation: {err}"
                    ),
                    Some(alias.clone()),
                    None,
                    Some(constraint.to_string()),
                    Some("catalog_validation"),
                )
            })?
            .into_iter()
            .collect::<HashSet<_>>();
        dimension_columns.insert(TableKey::new(alias), columns);
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
            source_columns_in_sink_equality_expr(expr, &source_qualifier_names, &sink_names);
        let implicit_uniqueness_columns =
            implicit_uniqueness_source_columns_expr(expr, &source_qualifier_names);
        let unaggregated = unaggregated_source_columns_expr(
            expr,
            registry,
            &source_qualifier_names,
            source_aliases,
        )?
        .into_iter()
        .filter(|qualified| !is_catalog_unique_source_column(catalog, qualified))
        .filter(|qualified| !implicit_uniqueness_columns.contains(&qualified.to_ascii_lowercase()))
        .filter(|qualified| !sink_equality_sources.contains(&qualified.to_ascii_lowercase()))
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
        let _ = source_columns_in_sink_equality_expr(expr, &source_qualifier_names, &sink_names);
    }

    let referenced_columns = parsed.qualified_columns.clone();

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
            dimension_queries,
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
    dimension_queries: &HashMap<String, String>,
    dimension_columns: &'a HashMap<TableKey, HashSet<String>>,
) -> Option<&'a HashSet<String>> {
    if dimension_tables
        .iter()
        .any(|table| TableKey::new(table) == *table_key)
    {
        return dimension_columns.get(table_key);
    }
    if dimension_queries.contains_key(table_key.as_str()) {
        return dimension_columns.get(table_key);
    }
    if let Some(base) = dimension_aliases.get(table_key.as_str()) {
        if dimension_queries.contains_key(base) {
            return dimension_columns.get(table_key);
        }
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

fn source_columns_in_sink_equality_expr(
    expr: &Expr,
    source_qualifier_names: &HashSet<TableKey>,
    sink_names: &HashSet<TableKey>,
) -> HashSet<String> {
    let mut found = HashSet::new();
    collect_source_sink_equality_pairs(expr, source_qualifier_names, sink_names, &mut found);
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

fn implicit_uniqueness_source_columns_expr(
    expr: &Expr,
    source_qualifier_names: &HashSet<TableKey>,
) -> HashSet<String> {
    let mut found = HashSet::new();
    collect_implicit_uniqueness_source_columns(expr, source_qualifier_names, &mut found);
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

fn unaggregated_source_columns_expr(
    expr: &Expr,
    registry: &AggregateRegistry,
    source_qualifier_names: &HashSet<TableKey>,
    source_aliases: &HashMap<String, String>,
) -> Result<Vec<String>, RewriteError> {
    Ok(UnaggregatedSourceColumnCollector::collect(
        expr,
        registry,
        source_qualifier_names,
        source_aliases,
    ))
}

struct UnaggregatedSourceColumnCollector<'a> {
    registry: &'a AggregateRegistry,
    source_qualifier_names: HashSet<TableKey>,
    found: Vec<String>,
    seen: HashSet<String>,
}

impl<'a> UnaggregatedSourceColumnCollector<'a> {
    fn collect(
        expr: &Expr,
        registry: &'a AggregateRegistry,
        source_qualifier_names: &HashSet<TableKey>,
        _source_aliases: &HashMap<String, String>,
    ) -> Vec<String> {
        let mut collector = Self {
            registry,
            source_qualifier_names: source_qualifier_names.clone(),
            found: Vec::new(),
            seen: HashSet::new(),
        };
        collector.visit_expr(expr);
        collector.found
    }

    fn visit_expr(&mut self, expr: &Expr) {
        self.visit_expr_with_context(expr, false, false);
    }

    fn visit_expr_with_context(
        &mut self,
        expr: &Expr,
        inside_aggregate: bool,
        inside_row_level: bool,
    ) {
        let shielded = inside_aggregate || inside_row_level;
        if let Some(column) = QualifiedColumn::from_expr(expr)
            && !shielded
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
            self.registry,
            |child, inside_aggregate, inside_row_level| {
                self.visit_expr_with_context(child, inside_aggregate, inside_row_level);
            },
            inside_aggregate,
            inside_row_level,
        );
    }
}

/// Built-in scalar functions that do not make source-column arguments row-level.
fn is_known_scalar_builtin(name: &str) -> bool {
    const KNOWN: &[&str] = &[
        "abs",
        "ceil",
        "ceiling",
        "char_length",
        "coalesce",
        "concat",
        "date_trunc",
        "day",
        "extract",
        "floor",
        "greatest",
        "ifnull",
        "isnull",
        "least",
        "len",
        "length",
        "lower",
        "ltrim",
        "md5",
        "month",
        "nullif",
        "nvl",
        "octet_length",
        "regexp_replace",
        "replace",
        "round",
        "rtrim",
        "sha256",
        "sqrt",
        "strpos",
        "substr",
        "substring",
        "to_char",
        "to_date",
        "to_timestamp",
        "trim",
        "try_cast",
        "upper",
        "year",
        "cast",
    ];
    let key = normalize_name(name);
    KNOWN.iter().any(|builtin| key == *builtin)
}

fn expr_visit_children(
    expr: &Expr,
    registry: &AggregateRegistry,
    mut visit: impl FnMut(&Expr, bool, bool),
    inside_aggregate: bool,
    inside_row_level: bool,
) {
    match expr {
        Expr::UnaryOp { expr, .. } => visit(expr, inside_aggregate, inside_row_level),
        Expr::BinaryOp { left, right, .. } => {
            visit(left, inside_aggregate, inside_row_level);
            visit(right, inside_aggregate, inside_row_level);
        }
        Expr::Nested(inner) => visit(inner, inside_aggregate, inside_row_level),
        Expr::Function(function) => {
            let name = function_name(function);
            if registry.is_aggregate_call(function) {
                visit_function_args(&function.args, &mut visit, true, false);
            } else if is_known_scalar_builtin(&name) {
                visit_function_args(&function.args, &mut visit, inside_aggregate, false);
            } else {
                // Extension / unknown scalars: arguments are row-level predicate inputs.
                visit_function_args(&function.args, &mut visit, inside_aggregate, true);
            }
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(operand) = operand {
                visit(operand, inside_aggregate, inside_row_level);
            }
            for (condition, result) in conditions.iter().zip(results.iter()) {
                visit(condition, inside_aggregate, inside_row_level);
                visit(result, inside_aggregate, inside_row_level);
            }
            if let Some(else_result) = else_result {
                visit(else_result, inside_aggregate, inside_row_level);
            }
        }
        Expr::InSubquery { expr, .. } | Expr::InList { expr, .. } => {
            visit(expr, inside_aggregate, inside_row_level);
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            visit(expr, inside_aggregate, inside_row_level);
            visit(low, inside_aggregate, inside_row_level);
            visit(high, inside_aggregate, inside_row_level);
        }
        Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            visit(expr, inside_aggregate, inside_row_level)
        }
        Expr::Cast { expr, .. } => visit(expr, inside_aggregate, inside_row_level),
        _ => {}
    }
}

fn visit_function_args(
    args: &FunctionArguments,
    visit: &mut impl FnMut(&Expr, bool, bool),
    inside_aggregate: bool,
    inside_row_level: bool,
) {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr};
    match args {
        FunctionArguments::None | FunctionArguments::Subquery(_) => {}
        FunctionArguments::List(list) => {
            for arg in &list.args {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    }
                    | FunctionArg::ExprNamed {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } => visit(expr, inside_aggregate, inside_row_level),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate_registry::AggregateRegistry;
    use crate::policy::PolicyIr;
    use crate::sql::SqlDialect;

    fn sample_registry() -> AggregateRegistry {
        AggregateRegistry::for_dialect(SqlDialect::DuckDb)
    }

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
        let err = catalog
            .validate_policy(&policy, &sample_registry())
            .unwrap_err();
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
        let err = catalog
            .validate_policy(&policy, &sample_registry())
            .unwrap_err();
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
        let err = catalog
            .validate_policy(&policy, &sample_registry())
            .unwrap_err();
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
            .validate_policy(&policy, &sample_registry())
            .expect("dimension alias column should validate");
    }

    #[test]
    fn validates_constraint_columns_through_dimension_subquery_alias() {
        let catalog = sample_catalog();
        let mut dimension_aliases = HashMap::new();
        dimension_aliases.insert("u".to_string(), "u".to_string());
        let mut dimension_queries = HashMap::new();
        dimension_queries.insert(
            "u".to_string(),
            "(SELECT user_id FROM session_user)".to_string(),
        );
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases,
            dimension_queries,
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "max(foo.id) > 1 AND u.user_id = 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        catalog
            .validate_policy(&policy, &sample_registry())
            .expect("dimension subquery alias column should validate");
    }

    #[test]
    fn rejects_unknown_dimension_subquery_column() {
        let catalog = sample_catalog();
        let mut dimension_aliases = HashMap::new();
        dimension_aliases.insert("u".to_string(), "u".to_string());
        let mut dimension_queries = HashMap::new();
        dimension_queries.insert(
            "u".to_string(),
            "(SELECT user_id FROM session_user)".to_string(),
        );
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases,
            dimension_queries,
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "max(foo.id) > 1 AND u.missing = 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog
            .validate_policy(&policy, &sample_registry())
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownColumn);
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
        let err = catalog
            .validate_policy(&policy, &sample_registry())
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownColumn);
    }

    #[test]
    fn accepts_median_aggregate_on_source() {
        let catalog = sample_catalog();
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "median(foo.id) > 10".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        catalog
            .validate_policy(&policy, &sample_registry())
            .expect("median should be recognized as aggregate");
    }

    #[test]
    fn rejects_unaggregated_source_inside_scalar_function() {
        let catalog = sample_catalog();
        let policy = PolicyIr::Pgn {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "abs(foo.id) > 10".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog
            .validate_policy(&policy, &sample_registry())
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnaggregatedSourceColumn);
    }

    #[test]
    fn accepts_row_level_extension_scalar_on_source_column() {
        let mut catalog = sample_catalog();
        catalog.register_table("docs", vec!["id".into(), "text".into()]);
        let policy = PolicyIr::Pgn {
            sources: vec!["docs".into()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: HashMap::new(),
            constraint: "is_safe(docs.text)".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        catalog
            .validate_policy(&policy, &sample_registry())
            .expect("unknown scalar UDF args are row-level, not aggregate context");
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
            .validate_policy(&policy, &sample_registry())
            .expect("alias-qualified source column should validate");
    }
}
