use passant_core::{PassantRewriter, PolicyIr, Resolution};

use crate::common::{dfc_policy, dfc_policy_sink, dfc_policy_with, rewriter_with_policies};

#[test]
fn delete_policy_matches_sources_and_constraint() {
    let mut rewriter = rewriter_with_policies(&[
        dfc_policy(&["foo"], "max(foo.id) > 1"),
        dfc_policy(&["bar"], "max(bar.id) > 1"),
    ]);
    let removed = rewriter.delete_policy(
        Some(&["foo".to_string()]),
        None,
        Some("max(foo.id) > 1"),
        None,
        None,
    );
    assert!(removed);
    assert_eq!(rewriter.policies().len(), 1);
    assert_eq!(rewriter.policies()[0].sources(), &["bar".to_string()]);
}

#[test]
fn delete_policy_returns_false_when_no_match() {
    let mut rewriter = rewriter_with_policies(&[dfc_policy(&["foo"], "max(foo.id) > 1")]);
    let removed = rewriter.delete_policy(
        Some(&["foo".to_string()]),
        None,
        Some("max(foo.id) > 99"),
        None,
        None,
    );
    assert!(!removed);
    assert_eq!(rewriter.policies().len(), 1);
}

#[test]
fn delete_policy_can_match_sink_and_on_fail() {
    let mut rewriter = rewriter_with_policies(&[PolicyIr::Dfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Kill,
        description: Some("stop".to_string()),
    }]);
    let removed = rewriter.delete_policy(
        Some(&["foo".to_string()]),
        Some("reports"),
        Some("max(foo.id) > 1"),
        Some(Resolution::Kill),
        Some("stop"),
    );
    assert!(removed);
    assert!(rewriter.policies().is_empty());
}

#[test]
fn dfc_policies_json_roundtrip_filters_policy_type() {
    let rewriter = rewriter_with_policies(&[
        dfc_policy(&["foo"], "max(foo.id) > 1"),
        dfc_policy_sink(&["foo"], "reports", "sum(reports.total) > 100"),
    ]);
    assert_eq!(rewriter.dfc_policies().len(), 2);
    assert!(matches!(rewriter.dfc_policies()[0], PolicyIr::Dfc { .. }));
    assert!(matches!(rewriter.dfc_policies()[1], PolicyIr::Dfc { .. }));
}

#[test]
fn has_registered_policies_tracks_all_policy_types() {
    let empty = PassantRewriter::new();
    assert!(!empty.has_registered_policies());
    assert!(!empty.has_dfc_policies());
    let dfc = rewriter_with_policies(&[dfc_policy(&["foo"], "max(foo.id) > 1")]);
    assert!(dfc.has_registered_policies());
    assert!(dfc.has_dfc_policies());
}

#[test]
fn pgn_policies_json_roundtrip_preserves_source_text() {
    let mut rewriter = PassantRewriter::new();
    let text = "PGN OVER SOURCE foo SINK reports AGGREGATE sum(foo.amount) CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE";
    rewriter
        .register_policy_text(text)
        .expect("pgn policy text should register");
    assert_eq!(rewriter.pgn_policies().len(), 1);
    assert!(rewriter.has_registered_policies());
    assert!(!rewriter.has_dfc_policies());
    let pgn_policies = rewriter.pgn_policies();
    let stored = match &pgn_policies[0] {
        PolicyIr::NativePgn(pgn) => pgn,
        _ => panic!("expected NativePgn"),
    };
    assert_eq!(stored.source_text.as_deref(), Some(text));
}

#[test]
fn delete_policy_with_partial_filters() {
    let mut rewriter = rewriter_with_policies(&[
        dfc_policy_sink(&["foo"], "reports", "reports.id > 0"),
        dfc_policy_with(&["foo"], "max(foo.id) > 1", Resolution::Remove),
    ]);
    let removed = rewriter.delete_policy(None, Some("reports"), None, None, None);
    assert!(removed);
    assert_eq!(rewriter.policies().len(), 1);
    assert_eq!(rewriter.policies()[0].sink(), None);
}

#[test]
fn register_policy_text_roundtrip() {
    let mut rewriter = rewriter_with_policies(&[]);
    rewriter
        .register_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE")
        .expect("policy text should register");
    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("registered policy should rewrite");
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id > 1");
}
