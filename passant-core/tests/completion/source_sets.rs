//! Source-set annotation completion tests.

use passant_core::{PolicyIr, Resolution};

fn cross_source_policy() -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn outer_join_nullable_source_policy_with_source_sets() {
    let actual = rewrite(
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &[cross_source_policy()],
    );
    crate::common::assert_same_rows(
        &actual,
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id=foo.id WHERE foo.id IS NULL",
        "CREATE TABLE foo(id INTEGER, amount INTEGER); CREATE TABLE bar(id INTEGER); INSERT INTO foo(id) VALUES(1),(2),(2),(4),(NULL); INSERT INTO bar VALUES(1),(2),(2),(3),(NULL);",
    );
}

#[test]
fn full_outer_join_cross_source_policy_with_source_sets() {
    let actual = rewrite(
        "SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id",
        &[cross_source_policy()],
    );
    crate::common::assert_same_rows(
        &actual,
        "SELECT bar.id FROM bar FULL JOIN foo ON bar.id=foo.id WHERE foo.id IS NULL OR bar.id IS NULL",
        "CREATE TABLE foo(id INTEGER, amount INTEGER); CREATE TABLE bar(id INTEGER); INSERT INTO foo(id) VALUES(1),(2),(2),(4),(NULL); INSERT INTO bar VALUES(1),(2),(2),(3),(NULL);",
    );
}

#[test]
fn union_all_cross_source_policy_with_source_sets() {
    let actual = rewrite(
        "SELECT id FROM foo UNION ALL SELECT id FROM bar",
        &[PolicyIr::Pgn {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: None,
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(foo.id) > max(bar.id)".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    crate::common::assert_same_rows(
        &actual,
        "SELECT id FROM foo UNION ALL SELECT id FROM bar",
        "CREATE TABLE foo(id INTEGER, amount INTEGER); CREATE TABLE bar(id INTEGER); INSERT INTO foo(id) VALUES(1),(2),(2),(4),(NULL); INSERT INTO bar VALUES(1),(2),(2),(3),(NULL);",
    );
}

#[test]
fn except_cross_source_policy_with_source_sets() {
    let error = crate::common::rewriter_with_catalog(&[cross_source_policy()], family_catalog())
        .rewrite("SELECT id FROM bar EXCEPT SELECT id FROM foo")
        .expect_err("negative-side provenance unavailable")
        .to_string();
    assert!(
        error.contains("policy 0") && error.contains("negative-side SOURCE foo"),
        "{error}"
    );
}

#[test]
fn anti_join_probe_cross_source_policy_with_source_sets() {
    assert_rewrite(
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id",
        &[cross_source_policy()],
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id",
    );
}

#[test]
fn scope_flags_require_source_sets_for_cross_source_outer_join() {
    let result = plan_query(
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &[cross_source_policy()],
    );
    assert!(result.scope.requires_source_set_annotations);
}

#[test]
fn insert_sink_write_applies_per_tuple_source_sets() {
    let sql = rewrite(
        "INSERT INTO reports SELECT bar.id, foo.amount FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &[PolicyIr::Pgn {
            sources: vec!["bar".to_string(), "foo".to_string()],
            required_sources: Vec::new(),
            dimension_tables: Vec::new(),
            dimension_aliases: std::collections::HashMap::new(),
            dimension_queries: std::collections::HashMap::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            source_aliases: std::collections::HashMap::new(),
            constraint: "max(bar.id) > max(foo.amount)".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    let db = duckdb::Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE foo(id INTEGER,amount INTEGER); CREATE TABLE bar(id INTEGER); CREATE TABLE reports(id INTEGER,amount INTEGER); INSERT INTO foo VALUES(1,0),(2,9),(4,NULL); INSERT INTO bar VALUES(1),(2),(3),(3),(4);").unwrap();
    db.execute_batch(&sql).expect("execute checked INSERT");
    let mut stmt = db
        .prepare("SELECT id,amount FROM reports ORDER BY id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i32>(0)?, r.get::<_, Option<i32>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    // Both sources present: 1>0 passes, 2>9 and 4>NULL fail. Missing foo skips the policy.
    assert_eq!(rows, vec![(1, Some(0)), (3, None), (3, None)]);
}

// Explicit schema model for this test family (not inferred from policies).
fn family_catalog() -> passant_core::TableCatalog {
    crate::common::fixture_catalog(&[
        ("foo", &["id", "amount"]),
        ("bar", &["id"]),
        ("reports", &["id", "amount"]),
    ])
}

fn assert_rewrite(sql: &str, policies: &[passant_core::PolicyIr], expected: &str) {
    crate::common::assert_rewrite_with_catalog(sql, policies, expected, family_catalog());
}
fn plan_query(sql: &str, policies: &[passant_core::PolicyIr]) -> passant_core::PlanQueryResult {
    crate::common::plan_query_with_catalog(sql, policies, family_catalog())
}
fn rewrite(sql: &str, policies: &[passant_core::PolicyIr]) -> String {
    crate::common::rewrite_with_catalog(sql, policies, family_catalog())
}
