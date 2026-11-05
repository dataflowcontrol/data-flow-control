//! Opaque contributor states and SQL-backed aggregate algebra.
//!
//! SQL owns scalar arithmetic/equality; Rust owns state layout, contributor
//! identity, overlap rules, applicability and capture/merge/finalize planning.
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sqlparser::ast::{Expr, FunctionArguments, Statement, visit_expressions};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;

use crate::aggregate_registry::AggregateRegistry;
use crate::sql::{SqlDialect, parse_policy_expr_duckdb};

pub type StateResult<T> = Result<T, String>;

/// The host executes SQL, without interpreting any policy/state rules.
pub trait SqlExecutor {
    fn query_json(&mut self, sql: &str) -> StateResult<String>;
    fn describe_types(&mut self, sql: &str) -> StateResult<Vec<String>>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntermediateSpec {
    pub name: String,
    pub source: String,
    pub expression: String,
    pub aggregate: String,
}
impl IntermediateSpec {
    pub fn validate(&self) -> StateResult<()> {
        if !matches!(
            self.aggregate.as_str(),
            "unique" | "min" | "max" | "bool_and" | "bool_or" | "sum" | "count" | "avg"
        ) {
            return Err(format!(
                "Unsupported intermediate aggregate: {}",
                self.aggregate
            ));
        }
        if self.expression.trim() == "*" {
            return if self.aggregate == "count" {
                Ok(())
            } else {
                Err("Only COUNT supports *".into())
            };
        }
        scalar_expr(&self.expression)?;
        Ok(())
    }
    pub fn idempotent(&self) -> bool {
        matches!(
            self.aggregate.as_str(),
            "unique" | "min" | "max" | "bool_and" | "bool_or"
        )
    }
    fn width(&self) -> usize {
        if matches!(self.aggregate.as_str(), "unique" | "avg") {
            2
        } else {
            1
        }
    }
}

/// Value text is produced by SQL CAST, never a Python numeric/date conversion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SqlValue {
    pub sql_type: String,
    pub value: Option<String>,
}
impl SqlValue {
    pub fn sql(&self) -> String {
        match &self.value {
            None => format!("CAST(NULL AS {})", self.sql_type),
            Some(value) => {
                // SQL text cannot contain embedded NUL. UTF-8 hex transports
                // every VARCHAR/control character without parser truncation.
                let hex = value
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                format!("CAST(decode(from_hex('{hex}')) AS {})", self.sql_type)
            }
        }
    }
    fn null() -> Self {
        Self {
            sql_type: "VARCHAR".into(),
            value: None,
        }
    }
    fn zero() -> Self {
        Self {
            sql_type: "BIGINT".into(),
            value: Some("0".into()),
        }
    }
}

#[derive(Debug, Clone)]
struct SourceState {
    source: String,
    identities: BTreeSet<String>,
    identity_types: Vec<String>,
    values: Vec<Vec<SqlValue>>,
}
#[derive(Debug, Clone)]
pub struct IntermediateState {
    owner: u64,
    sources: Vec<SourceState>,
}
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
#[derive(Debug, Clone)]
pub struct IntermediatePlan {
    pub specs: Vec<IntermediateSpec>,
    pub sources: BTreeSet<String>,
    pub required_sources: BTreeSet<String>,
    owner: u64,
}

pub fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn encoded(expr: &str) -> String {
    format!("json_object('sql_type', typeof({expr}), 'value', CAST(({expr}) AS VARCHAR))")
}
fn scalar_expr(sql: &str) -> StateResult<Expr> {
    let expr = parse_policy_expr_duckdb(sql).map_err(|e| e.to_string())?;
    let registry = AggregateRegistry::for_dialect(SqlDialect::DuckDb);
    let invalid = visit_expressions(&expr, |node| {
        if matches!(
            node,
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
        ) {
            return ControlFlow::Break(());
        }
        if let Expr::Function(f) = node
            && (f.over.is_some()
                || registry.is_aggregate_call(f)
                || matches!(f.args, FunctionArguments::Subquery(_))
                || matches!(f.parameters, FunctionArguments::Subquery(_)))
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    if invalid.is_break() {
        return Err("Intermediate expressions must be contributor-local scalars; final predicate must be a scalar expression".into());
    }
    Ok(expr)
}

pub fn validate_transfer_type(sql_type: &str, identity: bool) -> StateResult<()> {
    let kind = sql_type.to_ascii_uppercase();
    let decimal = kind
        .strip_prefix("DECIMAL(")
        .and_then(|s| s.strip_suffix(')'))
        .is_some_and(|s| {
            let parts: Vec<_> = s.split(',').collect();
            parts.len() == 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        });
    if (!matches!(
        kind.as_str(),
        "BOOLEAN"
            | "TINYINT"
            | "SMALLINT"
            | "INTEGER"
            | "BIGINT"
            | "HUGEINT"
            | "UTINYINT"
            | "USMALLINT"
            | "UINTEGER"
            | "UBIGINT"
            | "UHUGEINT"
            | "FLOAT"
            | "REAL"
            | "DOUBLE"
            | "VARCHAR"
            | "BLOB"
    ) && !decimal)
        || (identity && matches!(kind.as_str(), "FLOAT" | "REAL" | "DOUBLE"))
    {
        return Err(format!(
            "Lossless intermediate transfer is not established for {sql_type}; use a lossless SQL scalar encoding or an integer contributor identity"
        ));
    }
    Ok(())
}

impl IntermediatePlan {
    pub fn new(
        specs: Vec<IntermediateSpec>,
        sources: Vec<String>,
        required_sources: Vec<String>,
    ) -> StateResult<Self> {
        let sources: BTreeSet<_> = sources.into_iter().collect();
        let required_sources: BTreeSet<_> = required_sources.into_iter().collect();
        if !required_sources.is_subset(&sources) {
            return Err("Required sources must be declared sources".into());
        }
        let mut names = BTreeSet::new();
        for spec in &specs {
            spec.validate()?;
            if !sources.contains(&spec.source) {
                return Err("Every intermediate must belong to a declared source".into());
            }
            if !names.insert(spec.name.clone())
                || (spec.aggregate == "unique"
                    && !names.insert(format!("{}__cardinality", spec.name)))
            {
                return Err("Intermediate names and cardinality columns must be unique".into());
            }
        }
        Ok(Self {
            specs,
            sources,
            required_sources,
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
        })
    }
    pub fn empty(&self) -> IntermediateState {
        IntermediateState {
            owner: self.owner,
            sources: Vec::new(),
        }
    }
    fn specs_for(&self, source: &str) -> Vec<&IntermediateSpec> {
        self.specs.iter().filter(|s| s.source == source).collect()
    }
    pub fn validate(&self, state: &IntermediateState) -> StateResult<()> {
        if state.owner != self.owner {
            return Err("State belongs to another policy plan or snapshot".into());
        }
        Ok(())
    }
    pub fn capture<E: SqlExecutor>(
        &self,
        db: &mut E,
        source: &str,
        query: &str,
        keys: &[String],
    ) -> StateResult<IntermediateState> {
        if !self.sources.contains(source) {
            return Err(format!("Undeclared source: {source}"));
        }
        let statements = Parser::parse_sql(&DuckDbDialect {}, query).map_err(|e| e.to_string())?;
        if statements.len() != 1 || !matches!(statements[0], Statement::Query(_)) {
            return Err("Capture requires one SELECT query".into());
        }
        let query = statements[0].to_string();
        if keys.is_empty() || keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
            return Err("Provide distinct contributor identity columns".into());
        }
        let specs = self.specs_for(source);
        let mut projection: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(n, k)| format!("{} AS k{n}", quote(k)))
            .collect();
        projection.extend(specs.iter().enumerate().map(|(n, s)| {
            format!(
                "{} AS v{n}",
                if s.expression.trim() == "*" {
                    "1"
                } else {
                    &s.expression
                }
            )
        }));
        let input = format!(
            "SELECT {} FROM ({query}) AS {}",
            projection.join(", "),
            quote(source)
        );
        let types = db.describe_types(&input)?;
        for (n, sql_type) in types.iter().enumerate() {
            validate_transfer_type(sql_type, n < keys.len())?;
        }
        let identity_types = types[..keys.len()].to_vec();
        let row_key = format!(
            "ROW({})",
            (0..keys.len())
                .map(|n| format!("k{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let identity = format!(
            "CAST(json_array({}) AS VARCHAR)",
            (0..keys.len())
                .map(|n| encoded(&format!("k{n}")))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let null_key = (0..keys.len())
            .map(|n| format!("k{n} IS NULL"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let metrics = specs
            .iter()
            .enumerate()
            .map(|(n, s)| {
                let fields = match s.aggregate.as_str() {
                    "unique" => vec![
                        format!("LEAST(COUNT(DISTINCT v{n}), 2)"),
                        format!("MIN(v{n})"),
                    ],
                    "avg" => vec![format!("SUM(v{n})"), format!("COUNT(v{n})")],
                    aggregate => vec![format!("{aggregate}(v{n})")],
                };
                format!(
                    "json_array({})",
                    fields
                        .iter()
                        .map(|f| encoded(f))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "WITH raw AS MATERIALIZED ({input}), contributors AS (SELECT DISTINCT * FROM raw) SELECT json_object('identities', COALESCE(LIST({identity}), []::VARCHAR[]), 'inconsistent', COUNT(*) <> COUNT(DISTINCT {row_key}), 'null_id', COALESCE(BOOL_OR({null_key}), FALSE), 'metrics', json_array({metrics})) FROM contributors"
        );
        #[derive(Deserialize)]
        struct Captured {
            identities: Vec<String>,
            inconsistent: bool,
            null_id: bool,
            metrics: Vec<Vec<SqlValue>>,
        }
        let data: Captured =
            serde_json::from_str(&db.query_json(&sql)?).map_err(|e| e.to_string())?;
        if data.inconsistent {
            return Err("One contributor identity has inconsistent expression values".into());
        }
        if data.null_id {
            return Err("Contributor identities must not contain NULL".into());
        }
        Ok(IntermediateState {
            owner: self.owner,
            sources: vec![SourceState {
                source: source.into(),
                identities: data.identities.into_iter().collect(),
                identity_types,
                values: data.metrics,
            }],
        })
    }
    pub fn merge<E: SqlExecutor>(
        &self,
        db: &mut E,
        states: &[&IntermediateState],
    ) -> StateResult<IntermediateState> {
        let mut by_source: BTreeMap<&str, Vec<&SourceState>> = BTreeMap::new();
        for state in states {
            self.validate(state)?;
            for part in &state.sources {
                by_source.entry(&part.source).or_default().push(part);
            }
        }
        let mut merged = Vec::new();
        for (source, mut parts) in by_source {
            if parts
                .iter()
                .any(|part| part.identity_types != parts[0].identity_types)
            {
                return Err("Contributor identity schema changed between captures".into());
            }
            parts.sort_by_key(|p| std::cmp::Reverse(p.identities.len()));
            let mut kept: Vec<&SourceState> = Vec::new();
            for part in parts {
                if kept
                    .iter()
                    .any(|p| p.identities == part.identities && p.values != part.values)
                {
                    return Err("Contributor snapshot changed between captures".into());
                }
                if !kept
                    .iter()
                    .any(|p| part.identities.is_subset(&p.identities))
                {
                    kept.push(part);
                }
            }
            let specs = self.specs_for(source);
            if specs.iter().any(|s| !s.idempotent())
                && kept.iter().enumerate().any(|(n, p)| {
                    kept[n + 1..]
                        .iter()
                        .any(|other| !p.identities.is_disjoint(&other.identities))
                })
            {
                return Err("Partially overlapping additive intermediate states cannot be merged without splitting an input; retain disjoint inputs instead".into());
            }
            let mut values = Vec::new();
            for (n, spec) in specs.iter().enumerate() {
                values.push(self.merge_value(
                    db,
                    spec,
                    &kept.iter().map(|p| &p.values[n]).collect::<Vec<_>>(),
                )?);
            }
            merged.push(SourceState {
                source: source.into(),
                identity_types: kept[0].identity_types.clone(),
                identities: kept
                    .iter()
                    .flat_map(|p| p.identities.iter().cloned())
                    .collect(),
                values,
            });
        }
        Ok(IntermediateState {
            owner: self.owner,
            sources: merged,
        })
    }
    fn merge_value<E: SqlExecutor>(
        &self,
        db: &mut E,
        spec: &IntermediateSpec,
        values: &[&Vec<SqlValue>],
    ) -> StateResult<Vec<SqlValue>> {
        let rows = values
            .iter()
            .map(|row| {
                format!(
                    "({})",
                    row.iter().map(SqlValue::sql).collect::<Vec<_>>().join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let aliases = (0..spec.width())
            .map(|n| format!("v{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let aggregation: Vec<String> = match spec.aggregate.as_str() {
            "unique" => vec![
                "CASE WHEN MAX(v0)=2 THEN 2 ELSE LEAST(COUNT(DISTINCT v1),2) END".into(),
                "MIN(v1)".into(),
            ],
            "avg" => vec!["SUM(v0)".into(), "SUM(v1)".into()],
            "count" => vec!["SUM(v0)".into()],
            f => vec![format!("{f}(v0)")],
        };
        let sql = format!(
            "SELECT json_array({}) FROM (VALUES {rows}) AS states({aliases})",
            aggregation
                .iter()
                .map(|a| encoded(a))
                .collect::<Vec<_>>()
                .join(", ")
        );
        serde_json::from_str(&db.query_json(&sql)?).map_err(|e| e.to_string())
    }
    /// SQL-typed finalized values; UNIQUE is [cardinality,value], others [value].
    pub fn values<E: SqlExecutor>(
        &self,
        db: &mut E,
        state: &IntermediateState,
    ) -> StateResult<BTreeMap<String, Vec<SqlValue>>> {
        self.validate(state)?;
        let mut result = BTreeMap::new();
        for source in &self.sources {
            let part = state.sources.iter().find(|p| &p.source == source);
            for (n, spec) in self.specs_for(source).iter().enumerate() {
                let value = match part {
                    None => {
                        if spec.aggregate == "unique" {
                            vec![SqlValue::zero(), SqlValue::null()]
                        } else if spec.aggregate == "count" {
                            vec![SqlValue::zero()]
                        } else {
                            vec![SqlValue::null()]
                        }
                    }
                    Some(part) => {
                        let raw = &part.values[n];
                        if spec.aggregate == "unique" {
                            vec![
                                raw[0].clone(),
                                if raw[0].value.as_deref() == Some("1") {
                                    raw[1].clone()
                                } else {
                                    SqlValue {
                                        sql_type: raw[1].sql_type.clone(),
                                        value: None,
                                    }
                                },
                            ]
                        } else if spec.aggregate == "avg" {
                            let sql = format!(
                                "SELECT json_array({})",
                                encoded(&format!("{} / NULLIF({},0)", raw[0].sql(), raw[1].sql()))
                            );
                            serde_json::from_str(&db.query_json(&sql)?)
                                .map_err(|e| e.to_string())?
                        } else {
                            raw.clone()
                        }
                    }
                };
                result.insert(spec.name.clone(), value);
            }
        }
        Ok(result)
    }
    pub fn applicable(&self, state: &IntermediateState) -> StateResult<bool> {
        self.validate(state)?;
        let present: BTreeSet<_> = state
            .sources
            .iter()
            .filter(|p| !p.identities.is_empty())
            .map(|p| p.source.clone())
            .collect();
        Ok(!self.required_sources.is_subset(&present) || self.sources.is_subset(&present))
    }
    pub fn evaluate<E: SqlExecutor>(
        &self,
        db: &mut E,
        state: &IntermediateState,
        predicate: &str,
        sink: BTreeMap<String, SqlValue>,
    ) -> StateResult<Option<bool>> {
        if !self.applicable(state)? {
            return Ok(None);
        }
        let present: BTreeSet<_> = state
            .sources
            .iter()
            .filter(|p| !p.identities.is_empty())
            .map(|p| p.source.clone())
            .collect();
        if !self.required_sources.is_subset(&present) {
            return Ok(Some(false));
        }
        let predicate = scalar_expr(predicate)?;
        let values = self.values(db, state)?;
        let mut fields = BTreeMap::new();
        for spec in &self.specs {
            let value = &values[&spec.name];
            if spec.aggregate == "unique" {
                fields.insert(format!("{}__cardinality", spec.name), value[0].clone());
                fields.insert(spec.name.clone(), value[1].clone());
            } else {
                fields.insert(spec.name.clone(), value[0].clone());
            }
        }
        for (name, value) in sink {
            if fields.insert(name.clone(), value).is_some() {
                return Err(format!("Sink column shadows intermediate: {name}"));
            }
        }
        let projection = if fields.is_empty() {
            "1".into()
        } else {
            fields
                .iter()
                .map(|(name, value)| format!("{} AS {}", value.sql(), quote(name)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let sql = format!("SELECT to_json(({predicate}) IS TRUE) FROM (SELECT {projection})");
        Ok(Some(
            serde_json::from_str(&db.query_json(&sql)?).map_err(|e| e.to_string())?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Db(duckdb::Connection);
    impl Db {
        fn new() -> Self {
            let db = duckdb::Connection::open_in_memory().unwrap();
            db.execute_batch("CREATE TABLE cases(id INTEGER, amount DECIMAL(30,2), category VARCHAR); INSERT INTO cases VALUES (1,10.00,'public'),(2,20.00,'secret'),(3,NULL,NULL);").unwrap();
            Self(db)
        }
    }
    impl SqlExecutor for Db {
        fn query_json(&mut self, sql: &str) -> StateResult<String> {
            self.0
                .query_row(sql, [], |row| row.get(0))
                .map_err(|e| e.to_string())
        }
        fn describe_types(&mut self, sql: &str) -> StateResult<Vec<String>> {
            let mut statement = self
                .0
                .prepare(&format!("DESCRIBE ({sql})"))
                .map_err(|e| e.to_string())?;
            let rows = statement
                .query_map([], |row| row.get(1))
                .map_err(|e| e.to_string())?;
            rows.map(|row| row.map_err(|e| e.to_string())).collect()
        }
    }
    fn spec(name: &str, expression: &str, aggregate: &str) -> IntermediateSpec {
        IntermediateSpec {
            name: name.into(),
            source: "cases".into(),
            expression: expression.into(),
            aggregate: aggregate.into(),
        }
    }
    fn capture(plan: &IntermediatePlan, db: &mut Db, predicate: &str) -> IntermediateState {
        plan.capture(
            db,
            "cases",
            &format!("SELECT * FROM cases WHERE {predicate}"),
            &["id".into()],
        )
        .unwrap()
    }
    #[test]
    fn native_merge_preserves_weighted_average_and_unique_conflict() {
        let mut db = Db::new();
        let plan = IntermediatePlan::new(
            vec![
                spec("average", "amount", "avg"),
                spec("category", "category", "unique"),
            ],
            vec!["cases".into()],
            vec![],
        )
        .unwrap();
        let a = capture(&plan, &mut db, "id=1");
        let b = capture(&plan, &mut db, "id IN (2,3)");
        let joined = plan.merge(&mut db, &[&a, &a, &b]).unwrap();
        let all = capture(&plan, &mut db, "TRUE");
        assert_eq!(
            plan.values(&mut db, &joined).unwrap(),
            plan.values(&mut db, &all).unwrap()
        );
        assert_eq!(
            plan.evaluate(
                &mut db,
                &joined,
                "average=15 AND category__cardinality=2",
                BTreeMap::new()
            )
            .unwrap(),
            Some(true)
        );
    }
    #[test]
    fn native_overlap_rejection_and_containment() {
        let mut db = Db::new();
        let plan = IntermediatePlan::new(
            vec![spec("total", "amount", "sum")],
            vec!["cases".into()],
            vec![],
        )
        .unwrap();
        let a = capture(&plan, &mut db, "id IN (1,2)");
        let b = capture(&plan, &mut db, "id IN (2,3)");
        assert!(
            plan.merge(&mut db, &[&a, &b])
                .unwrap_err()
                .contains("overlapping")
        );
        let all = capture(&plan, &mut db, "TRUE");
        let merged = plan.merge(&mut db, &[&a, &b, &all]).unwrap();
        assert_eq!(
            plan.values(&mut db, &merged).unwrap(),
            plan.values(&mut db, &all).unwrap()
        );
    }
    #[test]
    fn native_owner_and_required_source_checks() {
        let mut db = Db::new();
        let a = IntermediatePlan::new(vec![], vec!["cases".into()], vec!["cases".into()]).unwrap();
        let b = IntermediatePlan::new(vec![], vec!["cases".into()], vec!["cases".into()]).unwrap();
        assert!(
            a.merge(&mut db, &[&b.empty()])
                .unwrap_err()
                .contains("another policy")
        );
        assert_eq!(
            a.evaluate(&mut db, &a.empty(), "TRUE", BTreeMap::new())
                .unwrap(),
            Some(false)
        );
        let state = capture(&a, &mut db, "id=1");
        assert_eq!(
            a.evaluate(&mut db, &state, "TRUE", BTreeMap::new())
                .unwrap(),
            Some(true)
        );
    }
    #[test]
    fn native_capture_deduplicates_sql_identity_and_keeps_decimal_precision() {
        let mut db = Db::new();
        db.0.execute_batch("UPDATE cases SET amount=12345678901234567890.12 WHERE id=1;")
            .unwrap();
        let plan = IntermediatePlan::new(
            vec![spec("total", "amount", "sum")],
            vec!["cases".into()],
            vec![],
        )
        .unwrap();
        let state = plan
            .capture(
                &mut db,
                "cases",
                "SELECT cases.* FROM cases CROSS JOIN range(3) WHERE id=1",
                &["id".into()],
            )
            .unwrap();
        assert_eq!(
            plan.values(&mut db, &state).unwrap()["total"][0]
                .value
                .as_deref(),
            Some("12345678901234567890.12")
        );
        assert!(
            plan.capture(
                &mut db,
                "cases",
                "SELECT '2026-01-01 00:00:00.000000001'::TIMESTAMP_NS AS identity, * FROM cases",
                &["identity".into()]
            )
            .unwrap_err()
            .contains("TIMESTAMP_NS")
        );
    }
    #[test]
    fn changed_identity_sql_type_cannot_double_count_one_contributor() {
        let mut db = Db::new();
        let plan = IntermediatePlan::new(
            vec![spec("total", "amount", "sum")],
            vec!["cases".into()],
            vec![],
        )
        .unwrap();
        let a = capture(&plan, &mut db, "id=1");
        let b = plan
            .capture(
                &mut db,
                "cases",
                "SELECT id::BIGINT AS id, amount FROM cases WHERE id=1",
                &["id".into()],
            )
            .unwrap();
        assert!(
            plan.merge(&mut db, &[&a, &b])
                .unwrap_err()
                .contains("identity schema")
        );
    }
    #[test]
    fn native_values_round_trip_embedded_nul_and_quotes() {
        let mut db = Db::new();
        db.0.execute("UPDATE cases SET category=? WHERE id=1", ["a\0b'雪"])
            .unwrap();
        let plan = IntermediatePlan::new(
            vec![spec("category", "category", "unique")],
            vec!["cases".into()],
            vec![],
        )
        .unwrap();
        let state = capture(&plan, &mut db, "id=1");
        let merged = plan.merge(&mut db, &[&state, &state]).unwrap();
        let value = plan.values(&mut db, &merged).unwrap()["category"][1].clone();
        assert_eq!(value.value.as_deref(), Some("a\0b'雪"));
        let fetched: String =
            db.0.query_row(&format!("SELECT {}", value.sql()), [], |row| row.get(0))
                .unwrap();
        assert_eq!(fetched, "a\0b'雪");
    }
}
