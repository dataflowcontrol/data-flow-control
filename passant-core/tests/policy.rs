mod common;

use passant_core::{PolicyIr, Resolution, parse_policy_text};

#[test]
fn parse_pgn_pgn_policy_text() {
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
        PolicyIr::Pgn {
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
    assert_eq!(policy.constraint(), "max(f.id) > 1");
    let PolicyIr::Pgn { source_aliases, .. } = policy;
    assert_eq!(source_aliases.get("f"), Some(&"foo".to_string()));
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
        "SOURCE foo AS f SINK reports DIMENSION catalog_users U, catalog_roles R CONSTRAINT max(f.id) > 1 ON FAIL REMOVE",
    )
    .expect("policy should parse");

    assert_eq!(
        policy.dimension_tables(),
        &["catalog_users".to_string(), "catalog_roles".to_string()]
    );
    assert_eq!(
        policy.dimension_aliases().get("u").map(String::as_str),
        Some("catalog_users")
    );
    assert_eq!(policy.constraint(), "max(f.id) > 1");
}

#[test]
fn parse_rejects_legacy_aggregate_prefix() {
    let err = parse_policy_text(
        "AGGREGATE SOURCE foo SINK reports DIMENSION reports.region CONSTRAINT sum(reports.total) > 100 ON FAIL REMOVE",
    )
    .expect_err("legacy AGGREGATE prefix should be rejected");
    assert!(err.to_string().contains("AGGREGATE clause was removed"));
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
