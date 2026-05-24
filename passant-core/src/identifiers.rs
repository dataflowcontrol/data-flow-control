use std::collections::HashMap;
use std::fmt;

use sqlparser::ast::{Expr, Ident, ObjectName};

/// Normalized identifier key for case-insensitive lookup.
pub fn normalize_key(value: &str) -> String {
    if value.contains('.') {
        value
            .split('.')
            .map(normalize_ident_segment)
            .collect::<Vec<_>>()
            .join(".")
    } else {
        normalize_ident_segment(value)
    }
}

fn normalize_ident_segment(value: &str) -> String {
    let trimmed = value.trim();
    let unquoted = if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].replace("\"\"", "\"")
    } else {
        trimmed.to_string()
    };
    unquoted.to_ascii_lowercase()
}

/// SQL identifier with preserved spelling and quoting from the source AST.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SqlIdent {
    raw: String,
}

impl SqlIdent {
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    pub fn from_ident(ident: &Ident) -> Self {
        Self {
            raw: ident.to_string(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn key(&self) -> String {
        normalize_key(&self.raw)
    }
}

impl fmt::Display for SqlIdent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

/// Table reference, optionally schema-qualified (`schema.table`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableName {
    raw: String,
}

impl TableName {
    pub fn parse(name: &str) -> Self {
        Self {
            raw: name.trim().to_string(),
        }
    }

    pub fn from_object_name(name: &ObjectName) -> Self {
        Self {
            raw: name
                .0
                .iter()
                .map(|part| part.to_string())
                .collect::<Vec<_>>()
                .join("."),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn key(&self) -> String {
        normalize_key(&self.raw)
    }

    pub fn matches_name(&self, name: &str) -> bool {
        self.key() == normalize_key(name)
    }
}

impl fmt::Display for TableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

/// Policy source table name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceName(TableName);

impl SourceName {
    pub fn parse(name: &str) -> Self {
        Self(TableName::parse(name))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn key(&self) -> String {
        self.0.key()
    }
}

impl fmt::Display for SourceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Policy sink table name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SinkName(TableName);

impl SinkName {
    pub fn parse(name: &str) -> Self {
        Self(TableName::parse(name))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn key(&self) -> String {
        self.0.key()
    }
}

impl fmt::Display for SinkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Normalized, case-insensitive table lookup key for scope and applicability checks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableKey(String);

impl TableKey {
    pub fn new(name: &str) -> Self {
        Self(normalize_key(name))
    }

    pub fn from_table(table: &TableName) -> Self {
        Self(table.key())
    }

    pub fn from_source(source: &SourceName) -> Self {
        Self(source.key())
    }

    pub fn from_sink(sink: &SinkName) -> Self {
        Self(sink.key())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// First character of the normalized key plus `_`, for legacy column-prefix heuristics.
    pub fn single_char_prefix(name: &str) -> String {
        Self::new(name)
            .as_str()
            .chars()
            .next()
            .map(|ch| format!("{ch}_"))
            .unwrap_or_default()
    }
}

impl fmt::Display for TableKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unqualified column name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnName(SqlIdent);

impl ColumnName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(SqlIdent::new(name))
    }

    pub fn from_ident(ident: &Ident) -> Self {
        Self(SqlIdent::from_ident(ident))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn key(&self) -> String {
        self.0.key()
    }
}

impl fmt::Display for ColumnName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Qualified `table.column` reference parsed from SQL AST nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QualifiedColumn {
    pub table: TableName,
    pub column: ColumnName,
}

impl QualifiedColumn {
    pub fn new(table: impl Into<String>, column: impl Into<String>) -> Self {
        Self {
            table: TableName::parse(table.into().as_str()),
            column: ColumnName::new(column),
        }
    }

    pub fn from_compound_identifier(parts: &[Ident]) -> Option<Self> {
        if parts.len() < 2 {
            return None;
        }
        let column = ColumnName::from_ident(parts.last()?);
        let table = parts[..parts.len() - 1]
            .iter()
            .map(|part| part.to_string())
            .collect::<Vec<_>>()
            .join(".");
        Some(Self {
            table: TableName::parse(&table),
            column,
        })
    }

    pub fn from_expr(expr: &Expr) -> Option<Self> {
        match expr {
            Expr::CompoundIdentifier(parts) => Self::from_compound_identifier(parts),
            _ => None,
        }
    }

    pub fn lookup_key(&self) -> (String, String) {
        (self.table.key(), self.column.key())
    }

    pub fn display_sql(&self) -> String {
        format!("{}.{}", self.table, self.column)
    }

    pub fn table_and_column(&self) -> (String, String) {
        (
            self.table.as_str().to_string(),
            self.column.as_str().to_string(),
        )
    }
}

/// Table alias in a query scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Alias(SqlIdent);

impl Alias {
    pub fn new(name: impl Into<String>) -> Self {
        Self(SqlIdent::new(name))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn key(&self) -> String {
        self.0.key()
    }
}

impl fmt::Display for Alias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Base-table lookup key → query alias for column qualification rewrites.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AliasByBase {
    inner: HashMap<String, String>,
}

impl AliasByBase {
    pub fn insert(&mut self, base: &TableName, alias: &Alias) {
        self.inner.insert(base.key(), alias.as_str().to_string());
    }

    pub fn get(&self, base: &TableName) -> Option<&str> {
        self.inner.get(&base.key()).map(String::as_str)
    }

    pub fn get_by_table_key(&self, base_key: &str) -> Option<&str> {
        self.inner.get(&normalize_key(base_key)).map(String::as_str)
    }

    pub fn single(base: impl AsRef<str>, alias: impl AsRef<str>) -> Self {
        let base = TableName::parse(base.as_ref());
        let alias = Alias::new(alias.as_ref());
        let mut map = Self::default();
        map.insert(&base, &alias);
        map
    }

    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self { inner: map }
    }

    pub fn inverted(&self) -> Self {
        Self {
            inner: self.inverse_lookup(),
        }
    }

    /// Alias lookup key → normalized base-table key.
    pub fn inverse_lookup(&self) -> HashMap<String, String> {
        self.inner
            .iter()
            .map(|(base_key, alias)| (normalize_key(alias), base_key.clone()))
            .collect()
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.inner
    }
}

/// Stable policy identifier for finalization and explain metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PolicyId(String);

impl PolicyId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn from_aggregate_constraint(constraint: &str) -> Self {
        Self(format!("aggregate::{constraint}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PolicyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Extract the column name from an identifier or qualified reference.
pub fn column_name_from_expr(expr: &Expr) -> Option<ColumnName> {
    match expr {
        Expr::Identifier(ident) => Some(ColumnName::from_ident(ident)),
        Expr::CompoundIdentifier(parts) if !parts.is_empty() => {
            Some(ColumnName::from_ident(parts.last()?))
        }
        _ => None,
    }
}

/// Extract the table prefix from a qualified column reference.
pub fn table_name_from_column_expr(expr: &Expr) -> Option<TableName> {
    QualifiedColumn::from_expr(expr).map(|column| column.table)
}

impl fmt::Display for QualifiedColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.table, self.column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::DuckDbDialect;
    use sqlparser::parser::Parser;

    fn parse_expr(sql: &str) -> Expr {
        let mut statements =
            Parser::parse_sql(&DuckDbDialect {}, &format!("SELECT {sql}")).expect("parse");
        let sqlparser::ast::Statement::Query(query) = statements.remove(0) else {
            panic!("expected query");
        };
        let sqlparser::ast::SetExpr::Select(mut select) = *query.body else {
            panic!("expected select");
        };
        match select.projection.remove(0) {
            sqlparser::ast::SelectItem::UnnamedExpr(expr) => expr,
            _ => panic!("expected unnamed expr"),
        }
    }

    #[test]
    fn table_name_normalizes_case_for_lookup() {
        let left = TableName::parse("Foo");
        let right = TableName::parse("foo");
        assert_eq!(left.key(), right.key());
        assert_eq!(left.as_str(), "Foo");
    }

    #[test]
    fn qualified_column_parses_schema_table_column() {
        let expr = parse_expr("\"MySchema\".\"MyTable\".\"OrderID\"");
        let column = QualifiedColumn::from_expr(&expr).expect("qualified column");
        assert_eq!(column.table.as_str(), "\"MySchema\".\"MyTable\"");
        assert_eq!(column.column.as_str(), "\"OrderID\"");
        assert_eq!(
            column.lookup_key(),
            ("myschema.mytable".to_string(), "orderid".to_string())
        );
    }

    #[test]
    fn qualified_column_parses_two_part_reference() {
        let expr = parse_expr("foo.id_value");
        let column = QualifiedColumn::from_expr(&expr).expect("qualified column");
        assert_eq!(column.display_sql(), "foo.id_value");
    }

    #[test]
    fn table_name_matches_schema_qualified_names() {
        let table = TableName::parse("MySchema.MyTable");
        assert!(table.matches_name("myschema.mytable"));
        assert!(table.matches_name("\"MySchema\".\"MyTable\""));
    }

    #[test]
    fn column_name_from_expr_returns_last_segment() {
        let expr = parse_expr("foo.id_value");
        let name = column_name_from_expr(&expr).expect("column");
        assert_eq!(name.as_str(), "id_value");
        assert_eq!(name.key(), "id_value");
    }

    #[test]
    fn table_key_normalizes_case_and_schema() {
        assert_eq!(TableKey::new("Foo"), TableKey::new("foo"));
        assert_eq!(
            TableKey::new("\"S\".\"T\""),
            TableKey::from_table(&TableName::parse("s.t"))
        );
        assert_eq!(
            TableKey::from_source(&SourceName::parse("Foo")),
            TableKey::new("foo")
        );
    }
}
