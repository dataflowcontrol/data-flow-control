//! Connection-aware aggregate function registry (builtins + introspected UDAFs).

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use sqlparser::ast::{Expr, Function, ObjectName};

use std::str::FromStr;

use crate::sql::SqlDialect;

/// How an aggregate may participate in semiring / rewrite laws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateClassification {
    /// Safe for current semiring full-push laws (count, sum, min, max, bool_and, bool_or).
    DistributiveInline,
    /// Rewritten to sum/count (avg, mean).
    DecomposableInline,
    /// Has a tuple-level scan substitute (count variants, array_agg/list, count_if).
    ScanTransformable,
    /// Recognized aggregate but not safe for full-push inline.
    #[default]
    AggregateNonDistributive,
    /// Custom UDAF; aggregate for validation but non-distributive unless reclassified.
    UnknownCustomAggregate,
}

impl FromStr for AggregateClassification {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(parse_classification(Some(value)))
    }
}

/// Origin of an aggregate entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateFunctionSource {
    #[default]
    Builtin,
    Introspected,
    UserDeclared,
}

/// Serializable aggregate metadata from Python catalog snapshots.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AggregateFunctionSnapshot {
    pub name: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub classification: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Resolved aggregate metadata in the registry.
#[derive(Debug, Clone)]
pub struct AggregateFunction {
    pub name: String,
    pub schema: Option<String>,
    pub aliases: Vec<String>,
    pub classification: AggregateClassification,
    pub source: AggregateFunctionSource,
}

#[derive(Debug, Clone, Default)]
pub struct AggregateRegistry {
    entries: HashMap<String, AggregateFunction>,
    clickhouse_combinators: bool,
}

impl AggregateRegistry {
    pub fn for_dialect(dialect: SqlDialect) -> Self {
        let mut registry = Self {
            entries: HashMap::new(),
            clickhouse_combinators: dialect == SqlDialect::ClickHouse,
        };
        registry.load_dialect_builtins(dialect);
        registry
    }

    pub fn merge_introspected(mut self, snapshots: &[AggregateFunctionSnapshot]) -> Self {
        for snapshot in snapshots {
            self.insert_snapshot(snapshot);
        }
        self
    }

    pub fn register_user_aggregate(
        &mut self,
        name: impl Into<String>,
        schema: Option<String>,
        classification: AggregateClassification,
    ) {
        let name = normalize_name(&name.into());
        self.entries.insert(
            name.clone(),
            AggregateFunction {
                name,
                schema,
                aliases: Vec::new(),
                classification,
                source: AggregateFunctionSource::UserDeclared,
            },
        );
    }

    pub fn lookup(&self, name: &str) -> Option<&AggregateFunction> {
        let key = normalize_name(name);
        if let Some(entry) = self.entries.get(&key) {
            return Some(entry);
        }
        key.rsplit('.')
            .next()
            .and_then(|base| self.entries.get(&normalize_name(base)))
    }

    pub fn classification(&self, name: &str) -> AggregateClassification {
        self.lookup(name)
            .map(|entry| entry.classification)
            .unwrap_or(AggregateClassification::AggregateNonDistributive)
    }

    pub fn is_aggregate_name(&self, name: &str) -> bool {
        if self.lookup(name).is_some() {
            return true;
        }
        let key = normalize_name(name);
        if self.clickhouse_combinators {
            if let Some(base) = clickhouse_combinator_base(&key) {
                if self.lookup(&base).is_some() {
                    return true;
                }
            }
        }
        false
    }

    pub fn is_aggregate_call(&self, function: &Function) -> bool {
        self.is_aggregate_name(&function_name(function))
    }

    pub fn is_semiring_distributive(&self, name: &str) -> bool {
        matches!(
            self.classification(name),
            AggregateClassification::DistributiveInline
                | AggregateClassification::DecomposableInline
        )
    }

    pub fn is_scan_transformable(&self, name: &str) -> bool {
        let lower = normalize_name(name);
        if matches!(
            self.classification(&lower),
            AggregateClassification::ScanTransformable
        ) {
            return true;
        }
        lower.contains("array_agg")
            || lower == "list"
            || lower.contains("count_if")
            || lower.contains("countif")
            || lower.contains("approx_count_distinct")
            || lower.contains("approx_distinct")
            || is_count_like_name(&lower)
    }

    pub fn expr_contains_aggregate(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Function(function) => {
                if self.is_aggregate_call(function) {
                    return true;
                }
                self.function_args_contain_aggregate(&function.args)
            }
            Expr::BinaryOp { left, right, .. } => {
                self.expr_contains_aggregate(left) || self.expr_contains_aggregate(right)
            }
            Expr::Nested(expr)
            | Expr::UnaryOp { expr, .. }
            | Expr::IsFalse(expr)
            | Expr::IsNotFalse(expr)
            | Expr::IsTrue(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr) => self.expr_contains_aggregate(expr),
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                operand
                    .as_deref()
                    .is_some_and(|e| self.expr_contains_aggregate(e))
                    || conditions.iter().any(|e| self.expr_contains_aggregate(e))
                    || results.iter().any(|e| self.expr_contains_aggregate(e))
                    || else_result
                        .as_deref()
                        .is_some_and(|e| self.expr_contains_aggregate(e))
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.expr_contains_aggregate(expr)
                    || self.expr_contains_aggregate(low)
                    || self.expr_contains_aggregate(high)
            }
            Expr::InList { expr, list, .. } => {
                self.expr_contains_aggregate(expr)
                    || list.iter().any(|item| self.expr_contains_aggregate(item))
            }
            _ => false,
        }
    }

    fn function_args_contain_aggregate(&self, args: &sqlparser::ast::FunctionArguments) -> bool {
        use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};
        let FunctionArguments::List(list) = args else {
            return false;
        };
        list.args.iter().any(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
            | FunctionArg::Named {
                arg: FunctionArgExpr::Expr(expr),
                ..
            }
            | FunctionArg::ExprNamed {
                arg: FunctionArgExpr::Expr(expr),
                ..
            } => self.expr_contains_aggregate(expr),
            _ => false,
        })
    }
}

fn is_count_like_name(name: &str) -> bool {
    matches!(
        name,
        "count" | "count_star" | "approx_count_distinct" | "approx_distinct" | "regr_count"
    )
}

pub fn function_name(function: &Function) -> String {
    match &function.name {
        ObjectName(parts) => parts
            .iter()
            .map(|part| part.value.clone())
            .collect::<Vec<_>>()
            .join("."),
    }
}

pub fn normalize_name(name: &str) -> String {
    name.trim_matches('"').to_ascii_lowercase()
}

fn parse_classification(value: Option<&str>) -> AggregateClassification {
    match value.map(str::trim).map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "distributive_inline" => AggregateClassification::DistributiveInline,
        Some(ref s) if s == "decomposable_inline" => AggregateClassification::DecomposableInline,
        Some(ref s) if s == "scan_transformable" => AggregateClassification::ScanTransformable,
        Some(ref s) if s == "unknown_custom" || s == "unknown_custom_aggregate" => {
            AggregateClassification::UnknownCustomAggregate
        }
        Some(ref s) if s == "aggregate_non_distributive" => {
            AggregateClassification::AggregateNonDistributive
        }
        _ => AggregateClassification::AggregateNonDistributive,
    }
}

fn parse_source(value: Option<&str>) -> AggregateFunctionSource {
    match value.map(str::trim).map(|s| s.to_ascii_lowercase()) {
        Some(ref s) if s == "introspected" => AggregateFunctionSource::Introspected,
        Some(ref s) if s == "user_declared" => AggregateFunctionSource::UserDeclared,
        _ => AggregateFunctionSource::Introspected,
    }
}

impl AggregateRegistry {
    fn insert_snapshot(&mut self, snapshot: &AggregateFunctionSnapshot) {
        let name = normalize_name(&snapshot.name);
        if snapshot.classification.is_none() {
            if let Some(existing) = self.entries.get(&name) {
                if existing.source == AggregateFunctionSource::Builtin {
                    return;
                }
            }
        }
        let classification = match snapshot.classification.as_deref() {
            Some(_) => parse_classification(snapshot.classification.as_deref()),
            None => AggregateClassification::UnknownCustomAggregate,
        };
        let source = parse_source(snapshot.source.as_deref());
        let entry = AggregateFunction {
            name: name.clone(),
            schema: snapshot.schema.clone(),
            aliases: snapshot
                .aliases
                .iter()
                .map(|alias| normalize_name(alias))
                .collect(),
            classification,
            source,
        };
        self.entries.insert(name.clone(), entry.clone());
        if let Some(schema) = snapshot.schema.as_deref() {
            let qualified = format!("{}.{}", normalize_name(schema), name);
            self.entries.insert(qualified, entry.clone());
        }
        for alias in &snapshot.aliases {
            let alias_key = normalize_name(alias);
            self.entries.insert(
                alias_key,
                AggregateFunction {
                    name: name.clone(),
                    schema: snapshot.schema.clone(),
                    aliases: Vec::new(),
                    classification,
                    source,
                },
            );
        }
    }

    fn insert_builtin(
        &mut self,
        name: &str,
        classification: AggregateClassification,
        aliases: &[&str],
    ) {
        let name = normalize_name(name);
        let entry = AggregateFunction {
            name: name.clone(),
            schema: None,
            aliases: aliases.iter().map(|a| normalize_name(a)).collect(),
            classification,
            source: AggregateFunctionSource::Builtin,
        };
        self.entries.insert(name.clone(), entry.clone());
        for alias in aliases {
            self.entries.insert(normalize_name(alias), entry.clone());
        }
    }

    fn load_dialect_builtins(&mut self, dialect: SqlDialect) {
        load_common_distributive(self);
        match dialect {
            SqlDialect::DuckDb => load_duckdb(self),
            SqlDialect::SQLite => load_sqlite(self),
            SqlDialect::Postgres => load_postgres(self),
            SqlDialect::ClickHouse => load_clickhouse(self),
            SqlDialect::DataFusion => load_datafusion(self),
            SqlDialect::Umbra => load_umbra(self),
            SqlDialect::GenericAnsi => {}
        }
        load_global_aliases(self);
    }
}

fn load_common_distributive(registry: &mut AggregateRegistry) {
    for name in ["count", "sum", "min", "max", "bool_and", "bool_or", "every"] {
        registry.insert_builtin(name, AggregateClassification::DistributiveInline, &[]);
    }
    registry.insert_builtin(
        "avg",
        AggregateClassification::DecomposableInline,
        &["mean"],
    );
    registry.insert_builtin(
        "array_agg",
        AggregateClassification::ScanTransformable,
        &["list"],
    );
}

fn load_global_aliases(registry: &mut AggregateRegistry) {
    registry.insert_builtin("mean", AggregateClassification::DecomposableInline, &[]);
    registry.insert_builtin(
        "bool_and",
        AggregateClassification::DistributiveInline,
        &["every"],
    );
}

fn load_duckdb(registry: &mut AggregateRegistry) {
    for name in [
        "any_value",
        "arg_max",
        "arg_min",
        "argmax",
        "argmin",
        "first",
        "last",
        "string_agg",
        "product",
        "median",
        "quantile_cont",
        "quantile_disc",
        "approx_count_distinct",
        "histogram",
        "bit_and",
        "bit_or",
        "bit_xor",
        "stddev_pop",
        "stddev_samp",
        "var_pop",
        "var_samp",
        "mode",
    ] {
        registry.insert_builtin(name, AggregateClassification::AggregateNonDistributive, &[]);
    }
    registry.insert_builtin("list", AggregateClassification::ScanTransformable, &[]);
    registry.insert_builtin(
        "count_if",
        AggregateClassification::ScanTransformable,
        &["countif"],
    );
}

fn load_sqlite(registry: &mut AggregateRegistry) {
    for name in [
        "total",
        "group_concat",
        "median",
        "percentile",
        "percentile_cont",
        "percentile_disc",
        "json_group_array",
        "json_group_object",
    ] {
        registry.insert_builtin(name, AggregateClassification::AggregateNonDistributive, &[]);
    }
    registry.insert_builtin(
        "string_agg",
        AggregateClassification::AggregateNonDistributive,
        &[],
    );
}

fn load_postgres(registry: &mut AggregateRegistry) {
    for name in [
        "json_agg",
        "jsonb_agg",
        "json_object_agg",
        "jsonb_object_agg",
        "bit_xor",
        "bit_and",
        "bit_or",
        "xmlagg",
        "range_agg",
        "regr_count",
        "regr_avgx",
        "regr_avgy",
        "regr_slope",
        "regr_intercept",
        "mode",
        "percentile_cont",
        "percentile_disc",
        "any_value",
        "string_agg",
        "stddev_pop",
        "stddev_samp",
        "var_pop",
        "var_samp",
    ] {
        registry.insert_builtin(name, AggregateClassification::AggregateNonDistributive, &[]);
    }
}

fn load_clickhouse(registry: &mut AggregateRegistry) {
    for name in [
        "any",
        "anylast",
        "argmax",
        "argmin",
        "avgweighted",
        "corr",
        "covarpop",
        "covarsamp",
        "grouparray",
        "groupbitmap",
        "groupconcat",
        "median",
        "quantile",
        "quantileexact",
        "quantiletdigest",
        "sumkahan",
        "uniq",
        "uniqexact",
        "uniqcombined",
        "uniqtheta",
        "varpop",
        "varsamp",
        "first_value",
        "last_value",
    ] {
        registry.insert_builtin(name, AggregateClassification::AggregateNonDistributive, &[]);
    }
}

fn load_datafusion(registry: &mut AggregateRegistry) {
    for name in [
        "median",
        "first_value",
        "last_value",
        "var_pop",
        "var_samp",
        "stddev_pop",
        "stddev_samp",
        "quantile_cont",
        "bit_and",
        "bit_or",
        "bit_xor",
        "grouping",
    ] {
        registry.insert_builtin(name, AggregateClassification::AggregateNonDistributive, &[]);
    }
}

fn load_umbra(registry: &mut AggregateRegistry) {
    load_postgres(registry);
    for name in ["percentile_disc", "bit_xor"] {
        registry.insert_builtin(name, AggregateClassification::AggregateNonDistributive, &[]);
    }
}

const CLICKHOUSE_COMBINATOR_SUFFIXES: &[&str] = &[
    "if",
    "ornull",
    "ornan",
    "distinct",
    "foreach",
    "map",
    "resample",
    "state",
    "merge",
    "simplestate",
];

fn clickhouse_combinator_base(name: &str) -> Option<String> {
    let lower = normalize_name(name);
    for suffix in CLICKHOUSE_COMBINATOR_SUFFIXES {
        if let Some(base) = lower.strip_suffix(suffix)
            && !base.is_empty()
        {
            return Some(base.to_string());
        }
    }
    None
}

/// Shared registry for tests and callers without a connection snapshot.
pub fn default_registry() -> Arc<AggregateRegistry> {
    Arc::new(AggregateRegistry::for_dialect(SqlDialect::DuckDb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdb_recognizes_median_and_list() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
        assert!(registry.is_aggregate_name("median"));
        assert!(registry.is_aggregate_name("list"));
        assert!(registry.is_aggregate_name("string_agg"));
    }

    #[test]
    fn rejects_scalar_functions() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
        assert!(!registry.is_aggregate_name("abs"));
        assert!(!registry.is_aggregate_name("lower"));
    }

    #[test]
    fn clickhouse_combinator_suffixes() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::ClickHouse);
        assert!(registry.is_aggregate_name("sumIf"));
        assert!(registry.is_aggregate_name("groupArrayDistinct"));
    }

    #[test]
    fn distributive_classification() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
        assert!(registry.is_semiring_distributive("sum"));
        assert!(registry.is_semiring_distributive("avg"));
        assert!(!registry.is_semiring_distributive("median"));
    }

    #[test]
    fn sqlite_builtins() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::SQLite);
        assert!(registry.is_aggregate_name("group_concat"));
        assert!(registry.is_aggregate_name("json_group_array"));
    }

    #[test]
    fn merge_introspected_custom_udaf() {
        let mut registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
        registry = registry.merge_introspected(&[AggregateFunctionSnapshot {
            name: "my_udaf".into(),
            schema: None,
            aliases: vec![],
            classification: Some("unknown_custom".into()),
            source: Some("introspected".into()),
        }]);
        assert!(registry.is_aggregate_name("my_udaf"));
        assert!(!registry.is_semiring_distributive("my_udaf"));
    }

    #[test]
    fn expr_contains_aggregate_inside_scalar_args() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
        let expr = crate::sql::parse_projection_expr("coalesce(sum(foo.x), 0)").unwrap();
        assert!(registry.expr_contains_aggregate(&expr));
    }

    #[test]
    fn lookup_schema_qualified_introspected_aggregate() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::Postgres).merge_introspected(&[
            AggregateFunctionSnapshot {
                name: "my_udaf".into(),
                schema: Some("public".into()),
                aliases: vec![],
                classification: Some("unknown_custom".into()),
                source: Some("introspected".into()),
            },
        ]);
        assert!(registry.is_aggregate_name("public.my_udaf"));
        let expr = crate::sql::parse_projection_expr("public.my_udaf(foo.x)").unwrap();
        let sqlparser::ast::Expr::Function(function) = expr else {
            panic!("expected function call");
        };
        assert!(registry.is_aggregate_call(&function));
    }

    #[test]
    fn merge_introspected_preserves_builtin_distributive_classification() {
        let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb).merge_introspected(&[
            AggregateFunctionSnapshot {
                name: "max".into(),
                schema: None,
                aliases: vec![],
                classification: None,
                source: Some("introspected".into()),
            },
        ]);
        assert!(registry.is_semiring_distributive("max"));
        assert!(registry.is_semiring_distributive("sum"));
    }
}
