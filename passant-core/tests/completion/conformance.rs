//! Conformance tests for stable rewriter behavior.

use passant_core::{PolicyIr, Resolution, parse_policy_text};

use crate::common::{assert_rewrite, pgn_policy, rewriter_with_policies};

#[test]
fn cross_source_outer_join_rewrites_with_source_sets() {
    let policies = vec![PolicyIr::Pgn {
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
    }];
    assert_rewrite(
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &policies,
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id",
    );
}

#[test]
fn cross_source_union_all_passes_through_when_branch_split_unavailable() {
    let policies = vec![PolicyIr::Pgn {
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
    }];
    assert_rewrite(
        "SELECT id FROM foo UNION ALL SELECT id FROM bar",
        &policies,
        "SELECT id FROM foo UNION ALL SELECT id FROM bar",
    );
}

#[test]
fn except_with_single_source_policy_rewrites_branch() {
    assert_rewrite(
        "SELECT id FROM bar EXCEPT SELECT id FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "SELECT id FROM bar EXCEPT SELECT id FROM foo WHERE foo.id > 1",
    );
}

#[test]
fn delete_with_policies_passthrough() {
    crate::common::assert_rewrite(
        "DELETE FROM foo WHERE id = 1",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "DELETE FROM foo WHERE id = 1",
    );
}

#[test]
fn create_table_as_select_fails_closed_with_registered_policies() {
    crate::common::assert_rewrite_fails_with(
        "CREATE TABLE leak AS SELECT * FROM foo",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "create_table",
    );
}

#[test]
fn copy_select_fails_closed_with_registered_policies() {
    crate::common::assert_rewrite_fails_with(
        "COPY (SELECT * FROM foo) TO 'out.csv' (HEADER)",
        &[pgn_policy(&["foo"], "max(foo.id) > 1")],
        "copy",
    );
}

#[test]
fn aggregate_policy_text_is_rejected() {
    let err = parse_policy_text(
        "AGGREGATE SOURCE foo SINK reports CONSTRAINT sum(reports.total) > 100 ON FAIL REMOVE",
    )
    .expect_err("AGGREGATE policies should be rejected");
    assert!(err.to_string().contains("AGGREGATE clause was removed"));
}

#[test]
fn anti_join_probe_side_policy_rewrites_with_source_sets() {
    let policies = vec![PolicyIr::Pgn {
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
    }];
    assert_rewrite(
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id",
        &policies,
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id WHERE foo.id > bar.id",
    );
}

#[test]
fn insert_without_required_source_rewrites_to_false() {
    let policy = PolicyIr::Pgn {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "reports.id > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    assert_rewrite(
        "INSERT INTO reports (id) SELECT other.id FROM other",
        &[policy],
        "INSERT INTO reports (id) SELECT other.id FROM other WHERE false",
    );
}

#[test]
fn duplicate_policy_registration_is_allowed() {
    let policy = pgn_policy(&["foo"], "max(foo.id) > 1");
    let rewriter = rewriter_with_policies(&[policy.clone(), policy]);
    assert_eq!(rewriter.policies().len(), 2);
    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("duplicate policies should still rewrite");
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id > 1 AND foo.id > 1");
}
