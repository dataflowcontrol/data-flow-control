//! FlowGuard / NativeFlowGuard completion tests.

use passant_core::{
    FlowGuardPolicy, FlowGuardPolicyKind, PolicyIr, PolicyScope, Resolution, parse_policy_text,
};

use crate::common::{assert_rewrite, rewriter_with_policies};

fn sample_flowguard_policy() -> PolicyIr {
    PolicyIr::NativeFlowGuard(FlowGuardPolicy {
        kind: FlowGuardPolicyKind::Over,
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
    })
}

#[test]
#[ignore = "completion: flowguard"]
fn parse_flowguard_policy_text_into_native_flowguard() {
    let policy = parse_policy_text(
        "FLOWGUARD OVER SOURCE foo SINK reports AGGREGATE sum(foo.amount) CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE",
    )
    .expect("flowguard policy text should parse");
    assert!(matches!(policy, PolicyIr::NativeFlowGuard(_)));
}

#[test]
#[ignore = "completion: flowguard"]
fn flowguard_policy_rewrites_insert_select() {
    assert_rewrite(
        "INSERT INTO reports SELECT id, amount FROM foo",
        &[sample_flowguard_policy()],
        "INSERT INTO reports SELECT id, amount FROM foo WHERE sum(foo.amount) <= 1000",
    );
}

#[test]
#[ignore = "completion: flowguard"]
fn flowguard_combined_with_compat_dfc_policy() {
    use passant_core::Resolution;

    let policies = vec![
        sample_flowguard_policy(),
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
#[ignore = "completion: flowguard"]
fn explain_includes_flowguard_policy_type() {
    use passant_core::PassantPlanner;

    let ir = passant_core::parse_query_to_ir("INSERT INTO reports SELECT id FROM foo")
        .expect("query should parse");
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &[sample_flowguard_policy()]);
    assert!(
        explanation
            .applicable_policies
            .iter()
            .any(|policy| policy.name() == "flowguard")
    );
}

#[test]
#[ignore = "completion: flowguard"]
fn flowguard_update_policy_kind() {
    let policy = PolicyIr::NativeFlowGuard(FlowGuardPolicy {
        kind: FlowGuardPolicyKind::Update,
        scope: PolicyScope {
            sources: vec!["foo".to_string()],
            sink: Some("reports".to_string()),
            sink_alias: None,
            dimensions: Vec::new(),
        },
        aggregations: vec!["sum(foo.amount)".to_string()],
        constraint: "sum(foo.amount) <= 1000".to_string(),
        on_fail: Resolution::Invalidate,
        description: None,
    });
    let rewriter = rewriter_with_policies(&[policy]);
    let sql = rewriter
        .rewrite("UPDATE reports SET amount = 100 FROM foo WHERE reports.id = foo.id")
        .expect("flowguard update rewrite should succeed");
    assert!(sql.contains("valid"));
}

#[test]
#[ignore = "completion: flowguard"]
fn flowguard_storage_roundtrip_via_policy_ir() {
    let rewriter = rewriter_with_policies(&[sample_flowguard_policy()]);
    assert_eq!(rewriter.policies().len(), 1);
    assert_eq!(rewriter.policies()[0].name(), "flowguard");
}
