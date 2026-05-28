mod common;

use passant_core::{PolicyIr, Resolution, parse_policy_text};

#[test]
fn parse_pgn_dfc_policy_text() {
    let policy = parse_policy_text(
        "SOURCE foo SINK reports CONSTRAINT max(foo.id) > 1 ON FAIL KILL DESCRIPTION stop bad rows",
    )
    .expect("policy should parse");

    assert_eq!(policy.sources(), &["foo".to_string()]);
    assert_eq!(policy.sink(), Some("reports"));
    assert_eq!(policy.constraint(), "max(foo.id) > 1");
    assert_eq!(policy.resolution(), Resolution::Kill);
}

#[test]
fn parse_pgn_sink_alias_policy_text() {
    let policy =
        parse_policy_text("SOURCE foo SINK reports AS r CONSTRAINT r.status = 'ok' ON FAIL REMOVE")
            .expect("policy should parse");

    assert_eq!(policy.sink(), Some("reports"));
    assert!(matches!(
        policy,
        PolicyIr::Dfc {
            sink_alias: Some(ref alias),
            ..
        } if alias == "r"
    ));
}

#[test]
fn parse_pgn_source_alias_policy_text() {
    let policy =
        parse_policy_text("SOURCE foo AS f SINK reports CONSTRAINT max(f.id) > 1 ON FAIL REMOVE")
            .expect("policy should parse");

    assert_eq!(policy.sources(), &["foo".to_string()]);
    assert_eq!(policy.constraint(), "max(foo.id) > 1");
}

#[test]
fn parse_pgn_required_source_policy_text() {
    let policy = parse_policy_text(
        "SOURCE REQUIRED receipts SINK reports CONSTRAINT reports.id > 0 ON FAIL REMOVE",
    )
    .expect("policy should parse");

    assert_eq!(policy.sources(), &["receipts".to_string()]);
    assert_eq!(policy.required_sources(), &["receipts".to_string()]);
    assert_eq!(policy.sink(), Some("reports"));
}

#[test]
fn parse_pgn_rejects_udf_resolution() {
    let err = parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL UDF")
        .expect_err("UDF resolution should be rejected");

    assert!(err.to_string().contains("invalid resolution: UDF"));
}

#[test]
fn parse_pgn_dimension_policy_text() {
    let policy = parse_policy_text(
        "SOURCE foo AS f SINK reports DIMENSION f.region, reports.department CONSTRAINT max(f.id) > 1 ON FAIL REMOVE",
    )
    .expect("policy should parse");

    assert_eq!(
        policy.dimensions(),
        &["foo.region".to_string(), "reports.department".to_string()]
    );
    assert_eq!(policy.constraint(), "max(foo.id) > 1");
}

#[test]
fn parse_rejects_aggregate_dfc_policy_text() {
    let err = parse_policy_text(
        "AGGREGATE SOURCE foo SINK reports DIMENSION reports.region CONSTRAINT sum(reports.total) > 100 ON FAIL REMOVE",
    )
    .expect_err("aggregate DFC policies should be rejected");
    assert!(
        err.to_string()
            .contains("aggregate policies are not supported")
    );
}

#[test]
fn parse_pgn_rejects_invalid_constraint_syntax() {
    let err = parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > ON FAIL REMOVE")
        .expect_err("policy should be invalid");

    assert!(
        err.to_string()
            .contains("invalid constraint SQL expression")
    );
}
