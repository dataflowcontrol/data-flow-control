mod common;
#[path = "common/duckdb.rs"]
mod duckdb_harness;
use duckdb::types::Value;
use duckdb_harness::TestDb;
use passant_core::tool_provenance::{ToolProvenanceModel, ToolPublicationPlan};
use passant_core::{PolicyIr, Resolution};

fn fixture() -> TestDb {
    let mut db = TestDb::new();
    db.exec("CREATE TABLE ToolInput(id INT,category VARCHAR,copies INT); INSERT INTO ToolInput VALUES(1,'public',2),(2,'private',1); CREATE TABLE ToolOutput(message VARCHAR)");
    db.rewriter.catalog_mut().register_table(
        "ToolInput",
        vec!["id".into(), "category".into(), "copies".into()],
    );
    db.rewriter
        .catalog_mut()
        .register_table("ToolOutput", vec!["message".into()]);
    db
}
fn policy(constraint: &str, resolution: Resolution) -> PolicyIr {
    let mut policy = common::pgn_policy_sink(&["ToolInput"], "ToolOutput", constraint);
    let PolicyIr::Pgn { on_fail, .. } = &mut policy;
    *on_fail = resolution;
    policy
}
fn plan(db: &TestDb, sql: &str, keys: &[&str], rows: usize) -> ToolPublicationPlan {
    let model =
        ToolProvenanceModel::new(sql, keys.iter().map(|s| s.to_string()).collect()).unwrap();
    db.rewriter
        .plan_tool_publication(
            &model,
            "ToolOutput",
            &vec![keys.len(); rows],
            &vec![vec!["message".into()]; rows],
            model.parameter_count(),
        )
        .unwrap()
}
fn scalar(db: &TestDb, sql: &str, values: &[Value]) -> i64 {
    db.conn
        .query_row(sql, duckdb::params_from_iter(values.iter()), |row| {
            row.get(0)
        })
        .unwrap_or_else(|err| panic!("query {sql}: {err}"))
}
fn s(value: &str) -> Value {
    Value::Text(value.into())
}
fn n(value: i32) -> Value {
    Value::Int(value)
}

#[test]
fn native_tool_constructor_and_shape_validation() {
    for sql in [
        "DELETE FROM ToolInput",
        "SELECT 1; SELECT 2",
        "SELECT random()",
        "SELECT uuid()",
        "SELECT $1",
        "SELECT id INTO copied FROM ToolInput",
    ] {
        assert!(ToolProvenanceModel::new(sql, vec![]).is_err(), "{sql}");
    }
    for exclusion in ["CURRENT ROW", "GROUP", "TIES", "NO OTHERS"] {
        let sql = format!(
            "SELECT SUM(id) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING EXCLUDE {exclusion}) FROM ToolInput"
        );
        assert!(
            ToolProvenanceModel::new(&sql, vec![])
                .unwrap_err()
                .to_string()
                .contains("window frame EXCLUDE")
        );
    }
    assert!(ToolProvenanceModel::new("SELECT 1 AS exclude GROUP BY exclude", vec![]).is_ok());
    let model = ToolProvenanceModel::new(
        "SELECT '?' AS literal, id FROM ToolInput WHERE id=?",
        vec!["id".into()],
    )
    .unwrap();
    assert_eq!(model.parameter_count(), 1);
    assert!(model.validate_parameter_count(0).is_err());
    assert!(
        model
            .validate_output_shapes(&[0], &[vec!["message".into()]])
            .is_err()
    );
    assert!(
        model
            .validate_output_shapes(&[1, 1], &[vec!["a".into()], vec!["b".into()]])
            .is_err()
    );
}

#[test]
fn native_tool_select_star_preserves_source_sink_and_parameter_binding() {
    let mut db = fixture();
    db.register_policy(policy(
        "ToolInput.category = ToolOutput.message",
        Resolution::Kill,
    ));
    let plan = plan(&db, "SELECT * FROM ToolInput WHERE id=?", &[], 1);
    let values = [n(1), s("public")];
    assert_eq!(scalar(&db, &plan.validation_sql, &values), 0);
    assert_eq!(
        scalar(&db, plan.publication_sql.as_ref().unwrap(), &values),
        1
    );
    assert_eq!(
        db.fetchall_strings("SELECT message FROM ToolOutput"),
        ["public"]
    );
}

#[test]
fn native_tool_lateral_one_to_many_checks_original_actual_values() {
    let mut db = fixture();
    db.register_policy(policy(
        "ToolInput.category = ToolOutput.message",
        Resolution::Kill,
    ));
    let plan = plan(
        &db,
        "SELECT i.*, n.copy FROM ToolInput i, LATERAL generate_series(1, i.copies) n(copy)",
        &["id", "copy"],
        3,
    );
    let values = [
        n(2),
        n(1),
        s("private"),
        n(1),
        n(2),
        s("public"),
        n(1),
        n(1),
        s("public"),
    ];
    assert_eq!(scalar(&db, &plan.validation_sql, &values), 0);
    assert_eq!(
        scalar(&db, plan.publication_sql.as_ref().unwrap(), &values),
        3
    );
}

#[test]
fn native_tool_correspondence_statuses_and_empty_outputs() {
    let db = fixture();
    let ordinary = plan(&db, "SELECT * FROM ToolInput", &["id"], 2);
    assert_eq!(
        scalar(&db, &ordinary.validation_sql, &[n(1), s("a"), n(1), s("b")]),
        2
    );
    assert_eq!(
        scalar(
            &db,
            &ordinary.validation_sql,
            &[Value::Null, s("a"), n(2), s("b")]
        ),
        1
    );
    assert_eq!(
        scalar(&db, &ordinary.validation_sql, &[n(1), s("a"), n(3), s("b")]),
        5
    );
    let duplicate = plan(&db, "SELECT 1 AS id FROM ToolInput", &["id"], 1);
    assert_eq!(scalar(&db, &duplicate.validation_sql, &[n(1), s("a")]), 4);
    let null = plan(&db, "SELECT NULL AS id", &["id"], 1);
    assert_eq!(scalar(&db, &null.validation_sql, &[n(1), s("a")]), 3);
    let empty = plan(&db, "SELECT * FROM ToolInput WHERE FALSE", &[], 0);
    assert_eq!(scalar(&db, &empty.validation_sql, &[]), 0);
    assert!(empty.publication_sql.is_none());
    assert!(
        ToolPublicationPlan::check_validation(4)
            .unwrap_err()
            .to_string()
            .contains("unique")
    );
    assert!(
        ToolPublicationPlan::check_validation(5)
            .unwrap_err()
            .to_string()
            .contains("match all")
    );
}

#[test]
fn native_tool_whole_group_and_window_remove_remain_intact() {
    let mut db = fixture();
    db.register_policy(policy("ToolInput.category='public'", Resolution::Remove));
    for (sql, keys, values, expected) in [
        (
            "SELECT SUM(copies) total FROM ToolInput",
            vec![],
            vec![s("secret-bearing aggregate")],
            0,
        ),
        (
            "SELECT id, SUM(copies) OVER (ORDER BY id) total FROM ToolInput",
            vec!["id"],
            vec![n(1), s("public"), n(2), s("secret-bearing frame")],
            1,
        ),
    ] {
        let rows = if keys.is_empty() { 1 } else { 2 };
        let plan = plan(&db, sql, &keys, rows);
        assert_eq!(scalar(&db, &plan.validation_sql, &values), 0);
        assert_eq!(
            scalar(&db, plan.publication_sql.as_ref().unwrap(), &values),
            expected
        );
    }
}

#[test]
fn native_tool_binding_does_not_capture_projected_where_aliases() {
    let mut db = fixture();
    db.exec("CREATE TABLE ModelInput(id INT,copies INT); INSERT INTO ModelInput VALUES(1,1),(1,2)");
    db.rewriter
        .catalog_mut()
        .register_table("ModelInput", vec!["id".into(), "copies".into()]);
    let bound = plan(
        &db,
        "SELECT id, copies AS v0 FROM ModelInput WHERE v0=1",
        &["id"],
        1,
    );
    let values = [n(1), s("1")];
    assert_eq!(scalar(&db, &bound.validation_sql, &values), 0);
    assert_eq!(
        scalar(&db, bound.publication_sql.as_ref().unwrap(), &values),
        1
    );
    let chained = plan(
        &db,
        "SELECT id, copies AS x, x+1 AS v0 FROM ModelInput WHERE v0=2",
        &["id"],
        1,
    );
    assert_eq!(scalar(&db, &chained.validation_sql, &values), 0);
    assert_eq!(
        scalar(&db, chained.publication_sql.as_ref().unwrap(), &values),
        1
    );
}

#[test]
fn native_tool_binding_preserves_business_columns_and_quoted_aliases() {
    let mut db = fixture();
    db.exec("CREATE TABLE ModelInput(id INT,k0 INT,v0 INT); INSERT INTO ModelInput VALUES(1,2,3)");
    db.rewriter
        .catalog_mut()
        .register_table("ModelInput", vec!["id".into(), "k0".into(), "v0".into()]);
    for alias in ["actual", "model", "__dfc_tool_actual", "__dfc_tool_model"] {
        let sql = format!("SELECT {alias}.* FROM ModelInput AS \"{alias}\" WHERE k0=2 AND v0=3");
        let plan = plan(&db, &sql, &["id"], 1);
        let values = [n(1), s("a")];
        assert_eq!(scalar(&db, &plan.validation_sql, &values), 0);
        assert_eq!(
            scalar(&db, plan.publication_sql.as_ref().unwrap(), &values),
            1
        );
    }
}

#[test]
fn native_tool_wildcard_accepts_unqualified_name_for_schema_table() {
    let mut db = fixture();
    db.rewriter.catalog_mut().register_table(
        "main.ToolInput",
        vec!["id".into(), "category".into(), "copies".into()],
    );
    let plan = plan(
        &db,
        "SELECT ToolInput.* FROM main.ToolInput WHERE id=1",
        &["id"],
        1,
    );
    let values = [n(1), s("a")];
    assert_eq!(scalar(&db, &plan.validation_sql, &values), 0);
    assert_eq!(
        scalar(&db, plan.publication_sql.as_ref().unwrap(), &values),
        1
    );
}

#[test]
fn native_tool_alias_expansion_preserves_source_column_precedence() {
    let db = fixture();
    let bound = plan(
        &db,
        "SELECT id, copies+1 AS copies FROM ToolInput WHERE copies=1",
        &["id"],
        1,
    );
    let values = [n(2), s("not a number")];
    assert_eq!(scalar(&db, &bound.validation_sql, &values), 0);
    assert_eq!(
        scalar(&db, bound.publication_sql.as_ref().unwrap(), &values),
        1
    );
}

#[test]
fn native_tool_preserves_derived_scope_without_column_binding_proof() {
    let db = fixture();
    db.exec("CREATE TABLE Uncatalogued(id INT,v0 INT); INSERT INTO Uncatalogued VALUES(1,1),(1,2)");
    let bound = plan(
        &db,
        "SELECT t.id FROM Uncatalogued t WHERE v0=1",
        &["id"],
        1,
    );
    let values = [n(1), s("1")];
    assert_eq!(scalar(&db, &bound.validation_sql, &values), 0);
    assert_eq!(
        scalar(&db, bound.publication_sql.as_ref().unwrap(), &values),
        1
    );
}

#[test]
fn native_tool_preserves_parameters_in_discarded_model_projections() {
    let db = fixture();
    for sql in [
        "SELECT id, ? AS context FROM ToolInput WHERE id=?",
        "SELECT id, ? FROM ToolInput WHERE id=?",
        "SELECT id, ? AS context, context + 1 AS derived FROM ToolInput WHERE id=?",
    ] {
        let bound = plan(&db, sql, &["id"], 1);
        let values = [n(7), n(1), n(1), s("payload")];
        assert_eq!(bound.model_parameter_count, 2);
        assert_eq!(scalar(&db, &bound.validation_sql, &values), 0);
        assert_eq!(scalar(&db, bound.publication_sql.as_ref().unwrap(), &values), 1);
    }
}

#[test]
fn native_tool_retains_parameters_used_by_flattened_keys_and_alias_filters() {
    let db = fixture();
    for sql in [
        "SELECT id+? AS key FROM ToolInput WHERE id=?",
        "SELECT id+? AS key, ? AS selected FROM ToolInput WHERE id=selected",
    ] {
        let bound = plan(&db, sql, &["key"], 1);
        let values = [n(10), n(1), n(11), s("payload")];
        assert_eq!(scalar(&db, &bound.validation_sql, &values), 0);
        assert_eq!(scalar(&db, bound.publication_sql.as_ref().unwrap(), &values), 1);
    }
}
