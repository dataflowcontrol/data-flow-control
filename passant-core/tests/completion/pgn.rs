//! PGN policy language completion tests.

use passant_core::{PolicyIr, parse_policy_text};

use crate::common::{assert_rewrite, pgn_policy, pgn_policy_sink, rewriter_with_policies};

fn sample_sink_policy() -> PolicyIr {
    pgn_policy_sink(&["foo"], "reports", "sum(foo.amount) <= 1000")
}

#[test]
fn parse_pgn_policy_text() {
    let policy = parse_policy_text(
        "SOURCE foo SINK reports CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE",
    )
    .expect("pgn policy text should parse");
    assert!(matches!(policy, PolicyIr::Pgn { .. }));
    assert_eq!(policy.constraint(), "sum(foo.amount) <= 1000");
}

#[test]
fn pgn_policy_rewrites_insert_select() {
    assert_rewrite(
        "INSERT INTO reports SELECT id, amount FROM foo",
        &[sample_sink_policy()],
        "INSERT INTO reports SELECT id, amount FROM foo WHERE foo.amount <= 1000",
    );
}

#[test]
fn pgn_combined_with_second_policy() {
    let policies = vec![
        sample_sink_policy(),
        pgn_policy(&["foo"], "max(foo.id) > 0"),
    ];
    assert_rewrite(
        "INSERT INTO reports SELECT id, amount FROM foo",
        &policies,
        "INSERT INTO reports SELECT id, amount FROM foo WHERE foo.amount <= 1000 AND foo.id > 0",
    );
}

#[test]
fn explain_includes_pgn_policy_type() {
    use passant_core::PassantPlanner;

    let ir = passant_core::parse_query_to_ir("INSERT INTO reports SELECT id FROM foo")
        .expect("query should parse");
    let explanation = PassantPlanner::new().explain_rewrite(&ir, &[sample_sink_policy()]);
    assert!(
        explanation
            .applicable_policies
            .iter()
            .any(|policy| policy.name() == "pgn")
    );
}

#[test]
fn pgn_policy_rewrites_update() {
    let policy = sample_sink_policy();
    let rewriter = rewriter_with_policies(&[policy]);
    let sql = rewriter
        .rewrite("UPDATE reports SET amount = 100 FROM foo WHERE reports.id = foo.id")
        .expect("pgn update rewrite should succeed");
    assert!(sql.contains("foo.amount <= 1000"));
}

#[test]
fn pgn_storage_roundtrip_via_policy_ir() {
    let rewriter = rewriter_with_policies(&[sample_sink_policy()]);
    assert_eq!(rewriter.policies().len(), 1);
    assert_eq!(rewriter.policies()[0].name(), "pgn");
}
