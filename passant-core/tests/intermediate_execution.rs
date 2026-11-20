use passant_core::intermediate_execution::{IntermediateResolution, IntermediateStatementPlan};
use passant_core::{CatalogSnapshot, PassantRewriter, Resolution};

fn planner() -> PassantRewriter {
    let mut planner = PassantRewriter::new();
    planner.apply_catalog_snapshot(serde_json::from_value::<CatalogSnapshot>(serde_json::json!({
        "tables": {"source": {"columns": ["id", "amount"]}, "target": {"columns": ["id", "value"]}},
        "intermediate_bindings": {"source": {"source": "source"}}
    })).unwrap());
    planner
}

#[test]
fn native_statement_plan_preserves_authority_and_logical_column_order() {
    let planner = planner();
    let IntermediateStatementPlan::Insert {
        target,
        columns,
        query_sql,
    } = planner
        .plan_intermediate_statement("INSERT INTO target (VALUE, ID) SELECT amount, id FROM source")
        .unwrap()
    else {
        panic!("expected insert")
    };
    assert_eq!(target, "target");
    assert_eq!(columns, ["value", "id"]);
    assert!(query_sql.contains("_dfc_intermediate_dependencies"));
    for sql in [
        "INSERT INTO source SELECT 1, 2",
        "CREATE TEMP TABLE SOURCE (id INT)",
        "INSERT INTO target(id) SELECT id FROM source",
        "INSERT INTO target(id,id) SELECT id,id FROM source",
        "INSERT INTO target VALUES (1,2)",
        "INSERT INTO target SELECT id, amount FROM source RETURNING *",
        "INSERT OR REPLACE INTO target SELECT id, amount FROM source",
        "CREATE TABLE x AS SELECT id AS _DFC_INTERMEDIATE_STATE FROM source",
        "CREATE TABLE x(_DFC_INTERMEDIATE_STATE INT)",
        "CREATE TABLE x LIKE target",
        "DELETE FROM target",
        "SELECT 1; SELECT 2",
    ] {
        assert!(planner.plan_intermediate_statement(sql).is_err(), "{sql}");
    }
}

#[test]
fn native_statement_plan_preserves_cte_capture_and_plain_ddl() {
    let planner = planner();
    let IntermediateStatementPlan::Create {
        target,
        temporary,
        query_sql,
        ddl_sql,
    } = planner
        .plan_intermediate_statement(
            "CREATE TEMP TABLE saved AS WITH a AS (SELECT * FROM source) SELECT * FROM a",
        )
        .unwrap()
    else {
        panic!("expected create")
    };
    assert_eq!(target, "saved");
    assert!(temporary && ddl_sql.is_none());
    assert!(query_sql.unwrap().contains("source:source"));
    let IntermediateStatementPlan::Create {
        query_sql, ddl_sql, ..
    } = planner
        .plan_intermediate_statement("CREATE TABLE saved(id INTEGER PRIMARY KEY)")
        .unwrap()
    else {
        panic!("expected create")
    };
    assert!(query_sql.is_none());
    assert!(ddl_sql.unwrap().contains("PRIMARY KEY"));
}

#[test]
fn resolution_registry_precedence_and_actual_retry_identities() {
    let mut planner = planner();
    planner
        .register_policy_text(
            "SOURCE source SINK target CONSTRAINT SUM(source.amount)>0 ON FAIL RETRY('same')",
        )
        .unwrap();
    planner
        .register_policy_text(
            "SOURCE source SINK target CONSTRAINT SUM(source.amount)>1 ON FAIL RETRY('same')",
        )
        .unwrap();
    planner
        .register_policy_text("SOURCE source SINK target CONSTRAINT source.id>0 ON FAIL REMOVE")
        .unwrap();
    planner
        .register_policy_text("SOURCE source SINK target CONSTRAINT source.id>1 ON FAIL KILL")
        .unwrap();
    let ids = planner.registered_policy_indices();
    let all = vec![Some(false); 4];
    assert_eq!(
        planner
            .resolve_intermediate_rows(Some("target"), &ids, &[all])
            .unwrap(),
        IntermediateResolution::Kill
    );
    let removed = vec![Some(false), Some(false), Some(false), Some(true)];
    assert_eq!(
        planner
            .resolve_intermediate_rows(Some("target"), &ids, &[removed])
            .unwrap(),
        IntermediateResolution::Keep { indices: vec![] }
    );
    let retry = vec![Some(false), Some(false), Some(true), Some(true)];
    let IntermediateResolution::Retry { failures } = planner
        .resolve_intermediate_rows(Some("target"), &ids, &[retry.clone(), retry])
        .unwrap()
    else {
        panic!("retry")
    };
    assert_eq!(
        failures
            .iter()
            .map(|f| (f.policy_index, f.message.as_str()))
            .collect::<Vec<_>>(),
        [(0, "same"), (1, "same")]
    );
    let killed_later = vec![
        vec![Some(false), None, Some(true), Some(true)],
        vec![None, None, Some(true), Some(false)],
    ];
    assert_eq!(
        planner
            .resolve_intermediate_rows(Some("target"), &ids, &killed_later)
            .unwrap(),
        IntermediateResolution::Kill
    );
    assert_eq!(
        planner
            .resolve_intermediate_rows(Some("other"), &ids, &[vec![Some(false); 4]])
            .unwrap(),
        IntermediateResolution::Keep { indices: vec![0] }
    );
}

#[test]
fn policy_replacement_and_verdict_shape_are_checked_in_core() {
    let mut planner = planner();
    let text = "SOURCE source SINK target CONSTRAINT source.id>0 ON FAIL RETRY('retry')";
    planner.register_policy_text(text).unwrap();
    let old = planner.registered_policy_indices();
    assert!(
        planner
            .resolve_intermediate_rows(Some("target"), &old, &[vec![]])
            .is_err()
    );
    assert!(planner.delete_policy(
        None,
        None,
        None,
        Some(Resolution::Retry("retry".into())),
        None
    ));
    planner.register_policy_text(text).unwrap();
    assert!(
        planner
            .resolve_intermediate_rows(Some("target"), &old, &[])
            .is_err()
    );
    let current = planner.registered_policy_indices();
    assert_eq!(current, [1]);
    assert_eq!(
        planner.intermediate_policy_indices(Some("TARGET")).unwrap(),
        [1]
    );
    assert!(
        planner
            .intermediate_policy_indices(None)
            .unwrap()
            .is_empty()
    );
}
