//! Source-set annotation completion tests.

use passant_core::{PolicyIr, Resolution};

use crate::common::{assert_rewrite, rewrite};

fn cross_source_policy() -> PolicyIr {
    PolicyIr::Dfc {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn outer_join_nullable_source_policy_with_source_sets() {
    assert_rewrite(
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &[cross_source_policy()],
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id",
    );
}

#[test]
fn full_outer_join_cross_source_policy_with_source_sets() {
    assert_rewrite(
        "SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id",
        &[cross_source_policy()],
        "SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id",
    );
}

#[test]
fn union_all_cross_source_policy_with_source_sets() {
    assert_rewrite(
        "SELECT id FROM foo UNION ALL SELECT id FROM bar",
        &[PolicyIr::Dfc {
            sources: vec!["foo".to_string(), "bar".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > max(bar.id)".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "SELECT id FROM foo UNION ALL SELECT id FROM bar",
    );
}

#[test]
fn except_cross_source_policy_with_source_sets() {
    let sql = rewrite(
        "SELECT id FROM bar EXCEPT SELECT id FROM foo",
        &[cross_source_policy()],
    );
    assert!(sql.contains("EXCEPT"));
    assert!(sql.contains("foo.id") && sql.contains("bar.id"));
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
    use crate::common::plan_query;

    let result = plan_query(
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &[cross_source_policy()],
    );
    assert!(result.scope.requires_source_set_annotations);
}

#[test]
fn insert_sink_write_applies_per_tuple_source_sets() {
    assert_rewrite(
        "INSERT INTO reports SELECT bar.id, foo.amount FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &[PolicyIr::Dfc {
            sources: vec!["bar".to_string(), "foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: Some("reports".to_string()),
            sink_alias: None,
            constraint: "max(bar.id) > max(foo.amount)".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        }],
        "INSERT INTO reports SELECT bar.id, foo.amount FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.amount",
    );
}
