//! Stable fail-closed behavior for unsupported query forms.
//!
//! These tests pass today and guard against silent regressions. When a feature
//! is implemented, convert the matching test into a positive rewrite test in the
//! same module and remove the conformance assertion.

use passant_core::{parse_policy_text, PolicyIr, Resolution};

use crate::common::{assert_rewrite_fails_with, dfc_policy, rewriter_with_policies};

#[test]
fn cross_source_outer_join_requires_source_set_annotations() {
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    assert_rewrite_fails_with(
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id",
        &policies,
        "outer join policy enforcement for nullable sources requires source-set annotations",
    );
}

#[test]
fn cross_source_set_operation_requires_source_set_annotations() {
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > max(bar.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    assert_rewrite_fails_with(
        "SELECT id FROM foo UNION ALL SELECT id FROM bar",
        &policies,
        "set operation policy enforcement requires source-set annotations",
    );
}

#[test]
fn except_with_policies_is_non_monotonic() {
    assert_rewrite_fails_with(
        "SELECT id FROM bar EXCEPT SELECT id FROM foo",
        &[dfc_policy(&["foo"], "max(foo.id) > 1")],
        "EXCEPT with registered policies is non-monotonic",
    );
}

#[test]
fn delete_with_policies_is_unsupported() {
    assert_rewrite_fails_with(
        "DELETE FROM foo WHERE id = 1",
        &[dfc_policy(&["foo"], "max(foo.id) > 1")],
        "delete with registered policies",
    );
}

#[test]
fn aggregate_policy_rejects_non_invalidate_resolution_at_parse() {
    let err = parse_policy_text(
        "AGGREGATE SOURCE foo SINK reports CONSTRAINT sum(reports.total) > 100 ON FAIL REMOVE",
    )
    .expect_err("aggregate policies should only support INVALIDATE");
    assert!(
        err.to_string()
            .contains("aggregate policies currently only support INVALIDATE resolution")
    );
}

#[test]
fn anti_join_probe_side_policy_requires_source_sets_when_not_prefiltered() {
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > max(bar.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];
    assert_rewrite_fails_with(
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id",
        &policies,
        "ANTI JOIN policy enforcement for probe-side sources requires source-set annotations",
    );
}

#[test]
fn insert_without_required_source_rewrites_to_false() {
    let policy = PolicyIr::CompatDfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.id > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    };
    crate::common::assert_rewrite(
        "INSERT INTO reports (id) SELECT other.id FROM other",
        &[policy],
        "INSERT INTO reports (id) SELECT other.id FROM other WHERE false",
    );
}

#[test]
fn duplicate_policy_registration_is_allowed() {
    let policy = dfc_policy(&["foo"], "max(foo.id) > 1");
    let rewriter = rewriter_with_policies(&[policy.clone(), policy]);
    assert_eq!(rewriter.policies().len(), 2);
    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("duplicate policies should still rewrite");
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id > 1 AND foo.id > 1");
}
