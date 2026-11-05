use passant_core::intermediate_compile::{IntermediateCompilation, bind_intermediate_sink};
use passant_core::intermediate_state::SqlValue;
use passant_core::{CatalogSnapshot, PassantRewriter};
use std::collections::BTreeMap;

fn compile(text: &str) -> Result<IntermediateCompilation, passant_core::RewriteError> {
    let mut planner = PassantRewriter::new();
    planner.apply_catalog_snapshot(
        serde_json::from_value::<CatalogSnapshot>(serde_json::json!({
            "tables": {
                "cases": {"columns": ["id", "category", "priority", "amount"]},
                "other": {"columns": ["amount"]},
                "target": {"columns": ["case_id", "expected", "success"]}
            }
        }))
        .unwrap(),
    );
    planner.register_policy_text(text)?;
    planner.compile_intermediate_policy(0)
}

#[test]
fn registered_ast_lowers_joint_expressions_before_aggregate_and_binds_sink() {
    let compiled = compile("SOURCE REQUIRED cases AS c SINK target AS t CONSTRAINT c.id=t.case_id AND BOOL_AND(c.category <> 'secret' OR c.priority='High') AND t.success ON FAIL REMOVE").unwrap();
    assert_eq!(compiled.sources, ["cases"]);
    assert_eq!(compiled.required_sources, ["cases"]);
    assert_eq!(compiled.specs.len(), 2);
    assert_eq!(compiled.specs[0].aggregate, "unique");
    assert_eq!(compiled.specs[1].aggregate, "bool_and");
    assert!(compiled.specs[1].expression.contains("OR"));
    assert!(!compiled.specs[1].expression.contains("c."));
    assert_eq!(
        compiled
            .sink_columns
            .iter()
            .map(|(_, c)| c.as_str())
            .collect::<Vec<_>>(),
        ["case_id", "success"]
    );
    assert!(compiled.predicate.contains("_metric_0__cardinality"));
}

#[test]
fn registered_avg_decomposition_and_joint_uniqueness_are_reused() {
    let compiled = compile("SOURCE cases SINK target CONSTRAINT cases.amount-cases.amount=0 AND AVG(cases.amount)=target.expected ON FAIL REMOVE").unwrap();
    assert_eq!(
        compiled
            .specs
            .iter()
            .map(|s| s.aggregate.as_str())
            .collect::<Vec<_>>(),
        ["unique", "sum", "count"]
    );
    assert!(compiled.specs[0].expression.contains('-'));
    assert!(compiled.predicate.contains('/'));
}

#[test]
fn unique_and_not_unique_use_capped_cardinality() {
    for prefix in ["UNIQUE", "NOT UNIQUE"] {
        let compiled = compile(&format!(
            "SOURCE cases CONSTRAINT {prefix} cases.category ON FAIL REMOVE"
        ))
        .unwrap();
        assert_eq!(compiled.specs[0].aggregate, "unique");
        assert!(compiled.predicate.contains("__cardinality"));
    }
}

#[test]
fn unsupported_demand_shapes_fail_closed() {
    for constraint in [
        "COUNT(DISTINCT cases.category)>4",
        "ARBITRARY(cases.category)='x'",
        "AVG(DISTINCT cases.amount)>0",
        "AVG(cases.amount) FILTER (WHERE cases.id>1)>0",
        "AVG(cases.amount) OVER ()>0",
        "SUM(cases.amount+other.amount)>0",
        "NOT(cases.amount>1)",
        "SUM(cases.amount) FILTER (WHERE cases.id IS NOT NULL)>0",
    ] {
        assert!(
            compile(&format!(
                "SOURCE cases, other CONSTRAINT {constraint} ON FAIL REMOVE"
            ))
            .is_err(),
            "{constraint}"
        );
    }
}

#[test]
fn sink_binding_validates_columns_in_rust() {
    let names = vec![("_sink_0".into(), "case_id".into())];
    assert!(bind_intermediate_sink(&names, &BTreeMap::new()).is_err());
    let value = SqlValue {
        sql_type: "VARCHAR".into(),
        value: Some("B".into()),
    };
    let supplied = BTreeMap::from([("case_id".into(), value.clone())]);
    assert_eq!(
        bind_intermediate_sink(&names, &supplied).unwrap(),
        BTreeMap::from([("_sink_0".into(), value)])
    );
}

struct Db(duckdb::Connection);
impl passant_core::intermediate_state::SqlExecutor for Db {
    fn query_json(&mut self, sql: &str) -> Result<String, String> {
        self.0
            .query_row(sql, [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }
    fn describe_types(&mut self, sql: &str) -> Result<Vec<String>, String> {
        let mut stmt = self
            .0
            .prepare(&format!("DESCRIBE ({sql})"))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get(1))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

#[test]
fn native_compiler_capture_merge_and_sink_evaluation_preserve_weighted_average() {
    use passant_core::intermediate_state::IntermediatePlan;
    let compiled = compile("SOURCE REQUIRED cases SINK target CONSTRAINT cases.amount-cases.amount=0 AND AVG(cases.amount)=target.expected AND BOOL_AND(cases.category <> 'secret' OR cases.priority='High') ON FAIL REMOVE").unwrap();
    let plan =
        IntermediatePlan::new(compiled.specs, compiled.sources, compiled.required_sources).unwrap();
    let mut db = Db(duckdb::Connection::open_in_memory().unwrap());
    db.0.execute_batch("CREATE TABLE cases(id INTEGER, amount INTEGER, category VARCHAR, priority VARCHAR); INSERT INTO cases VALUES (1,10,'public','Low'),(2,20,'secret','High'),(3,30,'public','Low');").unwrap();
    let first = plan
        .capture(
            &mut db,
            "cases",
            "SELECT * FROM cases WHERE id=1",
            &["id".into()],
        )
        .unwrap();
    let rest = plan
        .capture(
            &mut db,
            "cases",
            "SELECT * FROM cases WHERE id>1",
            &["id".into()],
        )
        .unwrap();
    let merged = plan.merge(&mut db, &[&first, &rest]).unwrap();
    let expected = BTreeMap::from([(
        "expected".into(),
        SqlValue {
            sql_type: "INTEGER".into(),
            value: Some("20".into()),
        },
    )]);
    let sink = bind_intermediate_sink(&compiled.sink_columns, &expected).unwrap();
    assert_eq!(
        plan.evaluate(&mut db, &merged, &compiled.predicate, sink)
            .unwrap(),
        Some(true)
    );
}

#[test]
fn uniqueness_guard_stays_in_its_boolean_branch() {
    use passant_core::intermediate_state::IntermediatePlan;
    let compiled = compile("SOURCE cases SINK target CONSTRAINT cases.id=target.case_id OR target.success ON FAIL REMOVE").unwrap();
    let plan =
        IntermediatePlan::new(compiled.specs, compiled.sources, compiled.required_sources).unwrap();
    let mut db = Db(duckdb::Connection::open_in_memory().unwrap());
    db.0.execute_batch("CREATE TABLE cases(id INTEGER); INSERT INTO cases VALUES (1),(2);")
        .unwrap();
    let state = plan
        .capture(&mut db, "cases", "SELECT * FROM cases", &["id".into()])
        .unwrap();
    for (success, expected) in [("true", true), ("false", false)] {
        let supplied = BTreeMap::from([
            (
                "case_id".into(),
                SqlValue {
                    sql_type: "INTEGER".into(),
                    value: Some("1".into()),
                },
            ),
            (
                "success".into(),
                SqlValue {
                    sql_type: "BOOLEAN".into(),
                    value: Some(success.into()),
                },
            ),
        ]);
        let sink = bind_intermediate_sink(&compiled.sink_columns, &supplied).unwrap();
        assert_eq!(
            plan.evaluate(&mut db, &state, &compiled.predicate, sink)
                .unwrap(),
            Some(expected)
        );
    }
}

#[test]
fn standalone_compilation_needs_no_existing_sink_or_source_catalog() {
    let policy = passant_core::policy::parse_policy_text(
        "SOURCE future SINK later CONSTRAINT future.id=later.id ON FAIL REMOVE",
    )
    .unwrap();
    let registry = passant_core::AggregateRegistry::for_dialect(passant_core::SqlDialect::DuckDb);
    let compiled =
        passant_core::intermediate_compile::compile_intermediate_spec(policy, &registry).unwrap();
    assert_eq!(compiled.sources, ["future"]);
    assert_eq!(compiled.sink_columns[0].1, "id");
}

#[test]
fn scalar_function_calls_use_existing_aggregate_registry_classification() {
    let compiled = compile("SOURCE cases CONSTRAINT CONCAT_WS('-', LOWER(cases.category), COALESCE(cases.priority,'none'))='public-low' ON FAIL REMOVE").unwrap();
    assert_eq!(compiled.specs.len(), 1);
    assert_eq!(compiled.specs[0].aggregate, "unique");
    assert!(compiled.specs[0].expression.contains("CONCAT_WS"));
}

#[test]
fn unknown_aggregate_macro_fails_capture_before_publishing_state() {
    use passant_core::intermediate_state::IntermediatePlan;
    let compiled =
        compile("SOURCE cases CONSTRAINT disguised_sum(cases.amount)>0 ON FAIL REMOVE").unwrap();
    let plan =
        IntermediatePlan::new(compiled.specs, compiled.sources, compiled.required_sources).unwrap();
    let mut db = Db(duckdb::Connection::open_in_memory().unwrap());
    db.0.execute_batch("CREATE TABLE cases(id INTEGER,amount INTEGER); INSERT INTO cases VALUES(1,10),(2,20); CREATE MACRO disguised_sum(x) AS SUM(x);").unwrap();
    let result = plan.capture(&mut db, "cases", "SELECT * FROM cases", &["id".into()]);
    assert!(
        result.is_err(),
        "macro aggregate cannot be a contributor-local scalar"
    );
}

#[test]
fn duckdb_arbitrary_alias_uses_first_aggregate_classification() {
    let registry = passant_core::AggregateRegistry::for_dialect(passant_core::SqlDialect::DuckDb);
    assert!(registry.is_aggregate_name("arbitrary"));
    assert_eq!(
        registry.classification("arbitrary"),
        registry.classification("first")
    );
    assert!(
        compile("SOURCE cases CONSTRAINT ARBITRARY(cases.category)='public' ON FAIL REMOVE")
            .is_err()
    );
}
