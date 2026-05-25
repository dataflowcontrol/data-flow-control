//! PGN / NativePgn completion tests.

use passant_core::{
    PgnPolicy, PgnPolicyKind, PolicyIr, PolicyScope, Resolution, parse_policy_text,
};

use crate::common::{assert_rewrite, rewriter_with_policies};

fn sample_pgn_policy() -> PolicyIr {
    PolicyIr::NativePgn(PgnPolicy {
        kind: PgnPolicyKind::Over,
        scope: PolicyScope {
            sources: vec!["foo".to_string()],
            sink: Some("reports".to_string()),
            sink_alias: None,
            dimensions: Vec::new(),
        },
        aggregations: vec!["sum(foo.amount)".to_string()],
        constraint: "sum(foo.amount) <= 1000".to_string(),
        on_fail: Resolution::Remove,
        description: None,
        source_text: None,
    })
}

#[test]
fn parse_pgn_policy_text_into_native_pgn() {
    let policy = parse_policy_text(
        "PGN OVER SOURCE foo SINK reports AGGREGATE sum(foo.amount) CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE",
    )
    .expect("pgn policy text should parse");
    assert!(matches!(policy, PolicyIr::NativePgn(_)));
}

#[test]
fn pgn_policy_rewrites_insert_select() {
    assert_rewrite(
        "INSERT INTO reports SELECT id, amount FROM foo",
        &[sample_pgn_policy()],
        "INSERT INTO reports SELECT id, amount FROM foo WHERE sum(foo.amount) <= 1000",
    );
}

#[test]
fn pgn_combined_with_compat_dfc_policy() {
    use passant_core::Resolution;

    let policies = vec![
        sample_pgn_policy(),
        PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 0".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        },
    ];
    assert_rewrite(
        "INSERT INTO reports SELECT id, amount FROM foo",
        &policies,
        "INSERT INTO reports SELECT id, amount FROM foo WHERE sum(foo.amount) <= 1000 AND foo.id > 0",
    );
}

#[test]
fn explain_includes_pgn_policy_type() {
    use passant_core::PassantPlanner;

    let ir = passant_core::parse_query_to_ir("INSERT INTO reports SELECT id FROM foo")
        .expect("query should parse");
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &[sample_pgn_policy()]);
    assert!(
        explanation
            .applicable_policies
            .iter()
            .any(|policy| policy.name() == "pgn")
    );
}

#[test]
fn pgn_update_policy_kind() {
    let policy = PolicyIr::NativePgn(PgnPolicy {
        kind: PgnPolicyKind::Update,
        scope: PolicyScope {
            sources: vec!["foo".to_string()],
            sink: Some("reports".to_string()),
            sink_alias: None,
            dimensions: Vec::new(),
        },
        aggregations: vec!["sum(foo.amount)".to_string()],
        constraint: "sum(foo.amount) <= 1000".to_string(),
        on_fail: Resolution::Remove,
        description: None,
        source_text: None,
    });
    let rewriter = rewriter_with_policies(&[policy]);
    let sql = rewriter
        .rewrite("UPDATE reports SET amount = 100 FROM foo WHERE reports.id = foo.id")
        .expect("pgn update rewrite should succeed");
    assert!(sql.contains("sum(foo.amount) <= 1000"));
}

#[test]
fn pgn_storage_roundtrip_via_policy_ir() {
    let rewriter = rewriter_with_policies(&[sample_pgn_policy()]);
    assert_eq!(rewriter.policies().len(), 1);
    assert_eq!(rewriter.policies()[0].name(), "pgn");
}
