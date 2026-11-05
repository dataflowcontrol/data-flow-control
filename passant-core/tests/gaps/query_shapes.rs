use passant_core::{PolicyIr, Resolution};

use crate::common::pgn_policy;

#[test]
fn cross_join_applies_root_where_filter() {
    assert_rewrite(
        "SELECT foo.id FROM foo CROSS JOIN baz",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "SELECT foo.id FROM foo CROSS JOIN baz WHERE foo.id > 1",
    );
}

#[test]
fn select_distinct_scan_applies_where_filter() {
    let sql = rewrite(
        "SELECT DISTINCT id FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id)>1")],
    );
    crate::common::assert_same_rows(
        &sql,
        "SELECT DISTINCT id FROM foo WHERE id>1",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); INSERT INTO foo VALUES(1,NULL),(2,NULL),(2,NULL),(NULL,NULL);",
    );
}

#[test]
fn select_distinct_aggregation_applies_having_filter() {
    assert_rewrite(
        "SELECT DISTINCT COUNT(*) FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "SELECT DISTINCT COUNT(*) FROM foo HAVING max(foo.id) > 1",
    );
}

#[test]
fn window_function_scan_preserves_over_clause() {
    let sql = rewrite(
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("ROW_NUMBER() OVER"));
    assert!(sql.contains("foo.id > 1"));
}

#[test]
fn correlated_exists_subquery_combines_outer_where_filter() {
    let sql = rewrite(
        "SELECT id FROM foo WHERE EXISTS (SELECT 1 FROM baz WHERE baz.x = foo.id)",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("EXISTS"));
    assert!(sql.contains("foo.id > 1"));
}

#[test]
fn in_list_predicate_combines_with_policy_filter() {
    let sql = rewrite(
        "SELECT id FROM foo WHERE id IN (1, 2, 3)",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
    );
    assert!(sql.contains("IN (1, 2, 3)"));
    assert!(sql.contains("foo.id > 1"));
}

#[test]
fn approx_count_distinct_equality_rewrites_to_one() {
    assert_rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "approx_count_distinct(foo.id) = 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "SELECT id FROM foo WHERE 1 = 1",
    );
}

#[test]
fn nested_aggregation_constraint_uses_inner_expression() {
    let sql = rewrite(
        "SELECT id FROM foo",
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "sum(max(foo.amount)) > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    assert!(
        sql.contains("foo.amount") || sql.contains("sum("),
        "expected nested aggregation fallback in rewrite: {sql}"
    );
}

// Explicit schema model for this test family (not inferred from policies).
fn family_catalog() -> passant_core::TableCatalog {
    crate::common::fixture_catalog(&[("foo", &["id", "amount"]), ("baz", &["x"])])
}

fn assert_rewrite(sql: &str, policies: &[passant_core::PolicyIr], expected: &str) {
    crate::common::assert_rewrite_with_catalog(sql, policies, expected, family_catalog());
}
fn rewrite(sql: &str, policies: &[passant_core::PolicyIr]) -> String {
    crate::common::rewrite_with_catalog(sql, policies, family_catalog())
}

#[test]
fn window_policy_kill_uses_frame_without_changing_window_results() {
    for frame in [
        "ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW",
        "PARTITION BY amount ORDER BY id",
        "ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING",
    ] {
        let query = format!("SELECT id, SUM(amount) OVER ({frame}) AS running FROM foo");
        let sql = rewrite(
            &query,
            &[crate::common::pgn_policy_kill(
                &["foo"],
                "COUNT(foo.id) <= 2",
            )],
        );
        assert!(
            sql.to_ascii_lowercase().contains(&format!(
                "count(foo.id) over ({})",
                frame.to_ascii_lowercase()
            )),
            "{sql}"
        );
        crate::common::assert_same_rows(
            &sql,
            &query,
            "CREATE TABLE foo(id INTEGER,amount INTEGER); \
             INSERT INTO foo VALUES(1,2),(2,3),(3,4); \
             CREATE MACRO passant_kill() AS error('policy violation');",
        );
    }
}

#[test]
fn window_policy_kill_rejects_contributor_aggregate_failure() {
    let sql = rewrite(
        "SELECT id, SUM(amount) OVER (ORDER BY id) AS running FROM foo",
        &[crate::common::pgn_policy_kill(
            &["foo"],
            "COUNT(foo.id) <= 1",
        )],
    );
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,2),(2,3); \
         CREATE MACRO passant_kill() AS error('policy violation');",
    )
    .unwrap();
    let mut statement = db.prepare(&sql).unwrap();
    assert!(statement.query([]).is_err(), "{sql}");
}

#[test]
fn window_policy_rejects_unsupported_contributor_shapes_before_filtering() {
    for (projection, resolution, message) in [
        (
            "LEAD(amount) OVER (ORDER BY id)",
            Resolution::Remove,
            "value/navigation windows",
        ),
        (
            "\"lead\"(amount) OVER (ORDER BY id)",
            Resolution::Kill,
            "value/navigation windows",
        ),
        (
            "FIRST_VALUE(amount) OVER (ORDER BY id DESC)",
            Resolution::Remove,
            "value/navigation windows",
        ),
        (
            "FILL(amount) OVER (ORDER BY id)",
            Resolution::Kill,
            "value/navigation windows",
        ),
        (
            "LAG(amount) OVER (ORDER BY id)",
            Resolution::Kill,
            "value/navigation windows",
        ),
        (
            "SUM(amount) OVER (ORDER BY id)",
            Resolution::Remove,
            "requires KILL",
        ),
        (
            "SUM(amount) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING)",
            Resolution::Kill,
            "including the current row",
        ),
        (
            "SUM(amount) OVER (ORDER BY id), MAX(amount) OVER ()",
            Resolution::Kill,
            "one common contributor frame",
        ),
    ] {
        let mut rewriter = passant_core::PassantRewriter::with_catalog(family_catalog());
        rewriter
            .register_policy(crate::common::pgn_policy_with(
                &["foo"],
                "MIN(foo.amount) > 0",
                resolution,
            ))
            .unwrap();
        let error = rewriter
            .rewrite(&format!("SELECT id, {projection} FROM foo"))
            .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
    }
}

fn whole_output_policy(constraint: &str) -> PolicyIr {
    let mut policy = pgn_policy(&["foo"], constraint);
    let PolicyIr::Pgn { on_fail, .. } = &mut policy;
    *on_fail = Resolution::Udf("__passant_remove_output".to_string());
    policy
}

#[test]
fn whole_output_remove_discards_group_without_recomputing_payload() {
    let sql = rewrite(
        "SELECT amount, SUM(id) AS total FROM foo GROUP BY amount",
        &[whole_output_policy("BOOL_AND(foo.id > 0)")],
    );
    assert!(!sql.contains("__passant_remove_output("), "{sql}");
    crate::common::assert_same_rows(
        &sql,
        "SELECT 20 AS amount, 3::HUGEINT AS total",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,10),(-1,10),(3,20);",
    );
}

#[test]
fn whole_output_remove_checks_original_window_frame() {
    let sql = rewrite(
        "SELECT id, SUM(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS total FROM foo",
        &[whole_output_policy("BOOL_AND(foo.amount > 0)")],
    );
    crate::common::assert_same_rows(
        &sql,
        "SELECT 1 AS id, 10::HUGEINT AS total UNION ALL SELECT 4, 70::HUGEINT",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,10),(2,-20),(3,30),(4,40);",
    );
}

#[test]
fn whole_output_remove_multiple_policies_share_original_candidates() {
    let sql = rewrite(
        "SELECT id, SUM(amount) OVER (ORDER BY id) AS total FROM foo",
        &[
            whole_output_policy("BOOL_AND(foo.amount > 0)"),
            whole_output_policy("COUNT(foo.id) = 2"),
        ],
    );
    // Removing id=2 must not make id=3 appear to have a two-row frame.
    crate::common::assert_same_rows(
        &sql,
        "SELECT id, amount AS total FROM foo WHERE false",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,10),(2,-20),(3,30);",
    );
}

#[test]
fn whole_output_remove_checks_both_self_join_contributors() {
    let sql = rewrite(
        "SELECT l.id, r.amount FROM foo l JOIN foo r ON l.amount = r.amount",
        &[whole_output_policy("BOOL_AND(foo.id > 0)")],
    );
    crate::common::assert_same_rows(
        &sql,
        "SELECT 1 AS id, 10 AS amount UNION ALL SELECT 3,20",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,10),(-1,10),(3,20);",
    );
}

#[test]
fn whole_output_remove_does_not_hide_kill_failure() {
    let sql = rewrite(
        "SELECT id, SUM(amount) OVER (ORDER BY id) AS total FROM foo",
        &[
            whole_output_policy("BOOL_AND(foo.amount > 0)"),
            crate::common::pgn_policy_kill(&["foo"], "COUNT(foo.id) <= 1"),
        ],
    );
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,10),(2,-20); \
         CREATE MACRO passant_kill() AS error('policy violation');",
    )
    .unwrap();
    let mut statement = db.prepare(&sql).unwrap();
    assert!(statement.query([]).is_err(), "{sql}");
}

#[test]
fn whole_output_remove_drops_unknown_policy_result() {
    let sql = rewrite(
        "SELECT amount, SUM(id) AS total FROM foo GROUP BY amount",
        &[whole_output_policy("BOOL_AND(foo.id > 0)")],
    );
    crate::common::assert_same_rows(
        &sql,
        "SELECT 20 AS amount, 3::HUGEINT AS total",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(NULL,10),(3,20);",
    );
}

#[test]
fn whole_output_remove_publisher_uses_scoped_legacy_boundary() {
    for (model, key, payloads) in [
        (
            "SELECT amount, SUM(id) AS total FROM foo GROUP BY amount",
            "amount",
            "(10,666),(20,777)",
        ),
        (
            "SELECT id, SUM(amount) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING) AS total FROM foo",
            "id",
            "(-1,666),(1,666),(3,777)",
        ),
    ] {
        let mut rewriter = passant_core::PassantRewriter::with_catalog(family_catalog());
        let mut policy = whole_output_policy("BOOL_AND((foo.id > 1) IS TRUE)");
        let PolicyIr::Pgn { sink, .. } = &mut policy;
        *sink = Some("baz".to_string());
        rewriter.register_policy(policy).unwrap();
        let query = format!(
            "INSERT INTO baz(x) SELECT actual.payload FROM ({model}) AS model \
             JOIN (VALUES {payloads}) actual(k,payload) ON model.{key}=actual.k"
        );
        // This is not a generally splittable resolution. The trusted adapter
        // proves one flat model plus a bijective binding before setting this
        // property on its temporary planner.
        assert!(rewriter.rewrite(&query).is_err());
        rewriter.register_resolution_splittability("__passant_remove_output", true);
        let sql = rewriter.rewrite(&query).unwrap();
        assert!(!sql.contains("__passant_remove_output("), "{sql}");
        let db = duckdb::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE foo(id INTEGER,amount INTEGER); CREATE TABLE baz(x INTEGER); \
             INSERT INTO foo VALUES(1,10),(-1,10),(3,20);",
        )
        .unwrap();
        db.execute_batch(&sql)
            .unwrap_or_else(|e| panic!("{e}: {sql}"));
        let values: Vec<i32> = db
            .prepare("SELECT x FROM baz")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(values, vec![777], "{sql}");
    }
}

#[test]
fn whole_output_publisher_checks_sink_only_policy_at_actual_payload_scope() {
    for payload in [777, 666] {
        let mut rewriter = passant_core::PassantRewriter::with_catalog(family_catalog());
        rewriter
            .register_policy(whole_output_policy("COUNT(foo.id) = 3"))
            .unwrap();
        let mut sink_policy = crate::common::pgn_policy_kill(&[], "baz.x = 777");
        let PolicyIr::Pgn { sink, .. } = &mut sink_policy;
        *sink = Some("baz".to_string());
        rewriter.register_policy(sink_policy).unwrap();
        rewriter.register_resolution_splittability("__passant_remove_output", true);
        let sql = rewriter
            .rewrite(&format!(
                "INSERT INTO baz(x) SELECT actual.v0 FROM \
                 (SELECT SUM(amount) AS total FROM foo) model \
                 JOIN (VALUES ({payload})) actual(v0) ON true"
            ))
            .unwrap();
        let db = duckdb::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE foo(id INTEGER,amount INTEGER); CREATE TABLE baz(x INTEGER); \
             INSERT INTO foo VALUES(1,10),(-1,10),(3,20); \
             CREATE MACRO passant_kill() AS error('policy violation');",
        )
        .unwrap();
        let result = db.execute_batch(&sql);
        if payload == 777 {
            result.unwrap_or_else(|e| panic!("{e}: {sql}"));
            assert_eq!(
                db.query_row("SELECT x FROM baz", [], |row| row.get::<_, i32>(0))
                    .unwrap(),
                777
            );
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("policy violation"), "{error}: {sql}");
        }
    }
}

#[test]
fn whole_output_scalar_policy_preserves_unique_and_rejects_null_contributors() {
    for source in ["foo", "foo AS input_alias"] {
        let sql = rewrite(
            &format!("SELECT amount, SUM(id) AS total FROM {source} GROUP BY amount"),
            &[whole_output_policy("foo.id > 0")],
        );
        assert!(sql.to_ascii_lowercase().contains("count(distinct"), "{sql}");
        crate::common::assert_same_rows(
            &sql,
            "SELECT 20 AS amount, 6::HUGEINT AS total",
            "CREATE TABLE foo(id INTEGER,amount INTEGER); \
             INSERT INTO foo VALUES(1,10),(2,10),(3,20),(3,20),(4,30),(NULL,30),(NULL,40);",
        );
    }
}

#[test]
fn whole_output_scalar_policy_checks_unique_in_original_window_frame() {
    let sql = rewrite(
        "SELECT amount, SUM(id) OVER (ORDER BY amount ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS total FROM foo",
        &[whole_output_policy("foo.id > 0")],
    );
    crate::common::assert_same_rows(
        &sql,
        "SELECT 10 AS amount, 1::HUGEINT AS total UNION ALL SELECT 30,4::HUGEINT",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,10),(2,20),(2,30),(NULL,40),(2,50);",
    );
}

#[test]
fn whole_output_scalar_policy_maps_unique_checks_to_self_join_aliases() {
    let sql = rewrite(
        "SELECT l.amount, SUM(l.id) AS total FROM foo l JOIN foo r ON l.amount=r.amount GROUP BY l.amount",
        &[whole_output_policy("foo.id > 0")],
    );
    crate::common::assert_same_rows(
        &sql,
        "SELECT 20 AS amount, 12::HUGEINT AS total",
        "CREATE TABLE foo(id INTEGER,amount INTEGER); \
         INSERT INTO foo VALUES(1,10),(2,10),(3,20),(3,20),(4,30),(NULL,30);",
    );
}
