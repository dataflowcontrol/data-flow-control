//! Symmetric self-join optimization completion tests (Section 4.7).

use passant_core::{PolicyIr, Resolution};

use crate::common::{plan_query, rewrite};

#[test]
fn self_join_three_aliases_applies_policy_once_per_occurrence_not_factorial() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    let result = plan_query(
        "SELECT a.id, b.id, c.id FROM foo AS a JOIN foo AS b ON a.id = b.id JOIN foo AS c ON b.id = c.id",
        std::slice::from_ref(&policy),
    );
    assert_eq!(result.applicable_policies.len(), 1);
    let sql = rewrite(
        "SELECT a.id, b.id, c.id FROM foo AS a JOIN foo AS b ON a.id = b.id JOIN foo AS c ON b.id = c.id",
        &[policy],
    );
    assert!(sql.matches("a.id > 1").count() == 1);
    assert!(sql.matches("b.id > 1").count() == 1);
    assert!(sql.matches("c.id > 1").count() == 1);
}

#[test]
fn explain_does_not_expand_policy_permutations_for_self_join() {
    use passant_core::PassantPlanner;

    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    let ir = passant_core::parse_query_to_ir(
        "SELECT a.id, b.id FROM foo AS a JOIN foo AS b ON a.id = b.id",
    )
    .expect("query should parse");
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &policies);
    assert_eq!(explanation.applicable_policies.len(), 1);
}

#[test]
fn self_join_execution_remove_filters_each_alias() {
    #[path = "../common/duckdb.rs"]
    mod duckdb;

    use duckdb::TestDb;

    let db = TestDb::new();
    db.exec("CREATE TABLE foo (id INT)");
    db.exec("INSERT INTO foo VALUES (1), (2), (3)");

    let sql = rewrite(
        "SELECT a.id FROM foo AS a JOIN foo AS b ON a.id = b.id",
        &[PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
    );
    let rows = db.fetchall_i64(&sql);
    assert_eq!(rows, vec![2, 3]);
}

#[test]
fn self_join_sink_write_preserves_alias_symmetry() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    let sql = rewrite(
        "INSERT INTO reports SELECT a.id FROM foo AS a JOIN foo AS b ON a.id = b.id",
        &[policy],
    );
    assert!(sql.contains("a.id > 1"));
    assert!(sql.contains("b.id > 1"));
}
