use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use sqlparser::ast::{Expr, Function, FunctionArguments, ObjectName};

use crate::diagnostics::{ErrorKind, RewriteError};
use crate::identifiers::{ColumnName, QualifiedColumn, SinkName, SourceName, TableKey};
use crate::policy::{AggregateDfcPolicy, PolicyIr, Resolution};
use crate::sql::parse_projection_expr;

/// DuckDB catalog facts supplied at policy registration time.
#[derive(Debug, Default, Clone)]
pub struct TableCatalog {
    table_columns: HashMap<TableKey, Vec<String>>,
    column_types: HashMap<(TableKey, String), String>,
    unique_columns: HashSet<(TableKey, String)>,
    loaded: bool,
}

/// JSON catalog snapshot from Python DuckDB introspection.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogSnapshot {
    pub tables: HashMap<String, CatalogTableInfo>,
    #[serde(default)]
    pub unique_columns: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogTableInfo {
    pub columns: Vec<String>,
    #[serde(default)]
    pub types: HashMap<String, String>,
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
        match policy {
            PolicyIr::CompatDfc {
                sources,
                dimensions,
                sink,
                sink_alias,
                constraint,
                on_fail,
                ..
            } => validate_dfc_policy(
                self,
                sources,
                dimensions,
                sink.as_deref(),
                sink_alias.as_deref(),
                constraint,
                *on_fail,
            ),
            PolicyIr::CompatAggregate(policy) => validate_aggregate_policy(self, policy),
            PolicyIr::NativePgn(_) => Ok(()),
        }
    }
}

/// Syntax validation for policy constraint and dimension expressions.
///
/// Parses the expression as SQL and requires all column references to be qualified.
pub fn validate_constraint_expression(sql: &str, label: &str) -> Result<(), RewriteError> {
    validate_qualified_columns(sql, label)
}

fn validate_dfc_policy(
    catalog: &TableCatalog,
    sources: &[String],
    dimensions: &[String],
    sink: Option<&str>,
    sink_alias: Option<&str>,
    constraint: &str,
    _on_fail: Resolution,
) -> Result<(), RewriteError> {
    let mut source_columns = HashMap::new();
    for source in sources {
        let source_name = SourceName::parse(source);
        if !catalog.table_exists(source_name.as_str()) {
            return Err(RewriteError::catalog(
                ErrorKind::UnknownTable,
                format!("Source table '{source}' does not exist"),
                Some(source.clone()),
                None,
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
            return Err(RewriteError::catalog(
                ErrorKind::UnknownTable,
                format!("Sink table '{sink}' does not exist"),
                Some(sink.to_string()),
                None,
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

    validate_qualified_columns(constraint, "constraint")?;
    for dimension in dimensions {
        validate_qualified_columns(dimension, "dimension")?;
    }

    let source_names = sources
        .iter()
        .map(|source| TableKey::new(source))
        .collect::<HashSet<_>>();
    let mut sink_names = HashSet::new();
    if let Some(sink) = sink {
        sink_names.insert(TableKey::new(sink));
    }
    sink_names.insert(TableKey::new("_output_"));
    if let Some(sink_alias) = sink_alias {
        sink_names.insert(TableKey::new(sink_alias));
    }

    if !sources.is_empty() {
        let unaggregated = unaggregated_source_columns(constraint, &source_names)?;
        if !unaggregated.is_empty() {
            return Err(RewriteError::catalog(
                ErrorKind::UnaggregatedSourceColumn,
                format!(
                    "Source columns in DFCPolicy constraints must be aggregated: {}",
                    unaggregated.join(", ")
                ),
                None,
                None,
            ));
        }
    }

    let mut referenced_columns = qualified_columns(constraint)?;
    for dimension in dimensions {
        referenced_columns.extend(qualified_columns(dimension)?);
    }

    for column in referenced_columns {
        let table_name = column.table.as_str();
        let column_name = column.column.as_str();
        let table_key = TableKey::from_table(&column.table);
        let column_key = column.column.key();
        if source_names.contains(&table_key) {
            let Some(columns) = source_columns.get(&table_key) else {
                continue;
            };
            if !columns.contains(&column_key) {
                return Err(RewriteError::catalog(
                    ErrorKind::UnknownColumn,
                    format!(
                        "Column '{table_name}.{column_name}' referenced in constraint \
                         does not exist in source table '{table_name}'"
                    ),
                    Some(table_name.to_string()),
                    Some(column_name.to_string()),
                ));
            }
        } else if sink_names.contains(&table_key) {
            if let Some(columns) = &sink_columns
                && !columns.contains(&column_key)
            {
                return Err(RewriteError::catalog(
                    ErrorKind::UnknownColumn,
                    format!(
                        "Column '{table_name}.{column_name}' referenced in constraint \
                         does not exist in sink table '{}'",
                        sink.unwrap_or(table_name)
                    ),
                    Some(table_name.to_string()),
                    Some(column_name.to_string()),
                ));
            }
        } else {
            return Err(RewriteError::catalog(
                ErrorKind::UnknownColumn,
                format!(
                    "Column '{table_name}.{column_name}' referenced in constraint \
                     references table '{table_name}', which is not in sources ({sources:?}) \
                     or sink ('{sink:?}')"
                ),
                Some(table_name.to_string()),
                Some(column_name.to_string()),
            ));
        }
    }

    Ok(())
}

fn validate_aggregate_policy(
    catalog: &TableCatalog,
    policy: &AggregateDfcPolicy,
) -> Result<(), RewriteError> {
    for source in &policy.sources {
        if !catalog.table_exists(source) {
            return Err(RewriteError::catalog(
                ErrorKind::UnknownTable,
                format!("Source table '{source}' does not exist"),
                Some(source.clone()),
                None,
            ));
        }
    }
    if let Some(sink) = &policy.sink
        && !catalog.table_exists(sink)
    {
        return Err(RewriteError::catalog(
            ErrorKind::UnknownTable,
            format!("Sink table '{sink}' does not exist"),
            Some(sink.clone()),
            None,
        ));
    }

    validate_qualified_columns(&policy.constraint, "constraint")?;
    for dimension in &policy.dimensions {
        validate_qualified_columns(dimension, "dimension")?;
    }

    let source_names = policy
        .sources
        .iter()
        .map(|source| TableKey::new(source))
        .collect::<HashSet<_>>();
    let mut sink_names = HashSet::new();
    if let Some(sink) = &policy.sink {
        sink_names.insert(TableKey::new(sink));
    }
    sink_names.insert(TableKey::new("_output_"));

    let mut referenced_columns = qualified_columns(&policy.constraint)?;
    for dimension in &policy.dimensions {
        referenced_columns.extend(qualified_columns(dimension)?);
    }

    for column in referenced_columns {
        let table_name = column.table.as_str();
        let column_name = column.column.as_str();
        let table_key = TableKey::from_table(&column.table);
        let column_key = column.column.key();
        if source_names.contains(&table_key) {
            let columns = catalog.columns(table_name).unwrap_or_default();
            let column_keys = columns
                .iter()
                .map(|col| ColumnName::new(col).key())
                .collect::<HashSet<_>>();
            if !column_keys.contains(&column_key) {
                return Err(RewriteError::catalog(
                    ErrorKind::UnknownColumn,
                    format!(
                        "Column '{table_name}.{column_name}' referenced in constraint \
                         does not exist in source table '{table_name}'"
                    ),
                    Some(table_name.to_string()),
                    Some(column_name.to_string()),
                ));
            }
        } else if sink_names.contains(&table_key) {
            if let Some(sink) = &policy.sink {
                let columns = catalog.columns(sink).unwrap_or_default();
                let column_keys = columns
                    .iter()
                    .map(|col| ColumnName::new(col).key())
                    .collect::<HashSet<_>>();
                if !column_keys.contains(&column_key) {
                    return Err(RewriteError::catalog(
                        ErrorKind::UnknownColumn,
                        format!(
                            "Column '{table_name}.{column_name}' referenced in constraint \
                             does not exist in sink table '{sink}'"
                        ),
                        Some(table_name.to_string()),
                        Some(column_name.to_string()),
                    ));
                }
            }
        } else {
            return Err(RewriteError::catalog(
                ErrorKind::UnknownColumn,
                format!(
                    "Column '{table_name}.{column_name}' referenced in constraint \
                     references table '{table_name}', which is not in sources ({:?}) \
                     or sink ('{:?}')",
                    policy.sources, policy.sink
                ),
                Some(table_name.to_string()),
                Some(column_name.to_string()),
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
        return Err(RewriteError::catalog(
            ErrorKind::UnqualifiedColumn,
            format!(
                "All columns in constraints and dimensions must be qualified with table names. \
                 Unqualified columns found: {}",
                unqualified.join(", ")
            ),
            None,
            None,
        ));
    }
    let _ = label;
    Ok(())
}

fn qualified_columns(sql: &str) -> Result<Vec<QualifiedColumn>, RewriteError> {
    let expr = parse_constraint_expr(sql)?;
    Ok(crate::sql::collect_qualified_columns_from_expr(&expr))
}

fn unaggregated_source_columns(
    sql: &str,
    source_names: &HashSet<TableKey>,
) -> Result<Vec<String>, RewriteError> {
    let expr = parse_constraint_expr(sql)?;
    Ok(UnaggregatedSourceColumnCollector::collect(
        &expr,
        source_names,
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
    source_names: HashSet<TableKey>,
    found: Vec<String>,
    seen: HashSet<String>,
}

impl UnaggregatedSourceColumnCollector {
    fn collect(expr: &Expr, source_names: &HashSet<TableKey>) -> Vec<String> {
        let mut collector = Self {
            source_names: source_names.clone(),
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
                .source_names
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
        catalog.register_table("reports", vec!["id".into(), "valid".into()]);
        catalog.register_column_type("reports", "valid", "BOOLEAN");
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
    }

    #[test]
    fn rejects_missing_source_table() {
        let catalog = sample_catalog();
        let policy = PolicyIr::CompatDfc {
            sources: vec!["missing".into()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
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
        let policy = PolicyIr::CompatDfc {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "foo.id > 0 AND avg(foo.id) > 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog.validate_policy(&policy).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnaggregatedSourceColumn);
    }

    #[test]
    fn rejects_missing_source_column() {
        let catalog = sample_catalog();
        let policy = PolicyIr::CompatDfc {
            sources: vec!["foo".into()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.missing) > 1".into(),
            on_fail: Resolution::Remove,
            description: None,
        };
        let err = catalog.validate_policy(&policy).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownColumn);
    }
}
