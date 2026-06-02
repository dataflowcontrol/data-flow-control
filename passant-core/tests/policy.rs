mod common;

use passant_core::{PolicyIr, PolicyParseError, Resolution, parse_policy_text};

#[test]
fn parse_pgn_rejects_empty_policy_text() {
    let err = parse_policy_text("   ").expect_err("empty policy should fail");
    assert!(matches!(err, PolicyParseError::Empty));
}

#[test]
fn parse_pgn_rejects_missing_constraint() {
    let err = parse_policy_text("SOURCE foo ON FAIL REMOVE").expect_err("missing constraint");
    assert!(matches!(err, PolicyParseError::MissingClause("CONSTRAINT")));
}

#[test]
fn parse_pgn_rejects_missing_on_fail() {
    let err =
        parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 1").expect_err("missing on fail");
    assert!(matches!(err, PolicyParseError::MissingClause("ON FAIL")));
}

#[test]
fn parse_pgn_sources_plural_and_multiple() {
    let policy = parse_policy_text("SOURCES foo, bar CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE")
        .expect("policy should parse");
    assert_eq!(policy.sources(), &["foo".to_string(), "bar".to_string()]);
}

#[test]
fn parse_pgn_source_none() {
    let policy = parse_policy_text("SOURCE NONE CONSTRAINT max(_OUTPUT_.id) > 0 ON FAIL REMOVE")
        .expect("policy should parse");
    assert!(policy.sources().is_empty());
}

#[test]
fn parse_pgn_sink_none() {
    let policy =
        parse_policy_text("SOURCE foo SINK NONE CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE")
            .expect("policy should parse");
    assert!(policy.sink().is_none());
}

#[test]
fn parse_pgn_sink_implicit_alias() {
    let policy =
        parse_policy_text("SOURCE foo SINK reports r CONSTRAINT r.status = 'ok' ON FAIL REMOVE")
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
fn parse_pgn_rejects_duplicate_sources() {
    let err = parse_policy_text("SOURCE foo, foo CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE")
        .expect_err("duplicate source");
    assert!(err.to_string().contains("duplicate source table 'foo'"));
}

#[test]
fn parse_pgn_required_source_inline_is_listed() {
    let policy = parse_policy_text("SOURCE REQUIRED bar CONSTRAINT max(bar.id) > 0 ON FAIL REMOVE")
        .expect("required inline source should parse");
    assert_eq!(policy.sources(), &["bar".to_string()]);
    assert_eq!(policy.required_sources(), &["bar".to_string()]);
}

#[test]
fn parse_pgn_dimension_subquery_with_alias() {
    let policy = parse_policy_text(
        "SOURCE foo DIMENSION (SELECT id FROM users) u CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE",
    )
    .expect("policy should parse");
    assert!(policy.dimension_queries().contains_key("u"));
}

#[test]
fn parse_pgn_output_reference_in_constraint() {
    let policy = parse_policy_text(
        "SOURCE foo SINK reports CONSTRAINT _OUTPUT_.status = 'ok' ON FAIL REMOVE",
    )
    .expect("policy should parse");
    assert_eq!(policy.constraint(), "_OUTPUT_.status = 'ok'");
}

#[test]
fn parse_pgn_ui_resolution() {
    let policy = parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL UI")
        .expect("policy should parse");
    assert_eq!(policy.resolution(), Resolution::Ui);
}

#[test]
fn parse_pgn_udf_resolution() {
    let policy =
        parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL UDF keep_positive")
            .expect("policy should parse");
    assert_eq!(
        policy.resolution(),
        Resolution::Udf("keep_positive".to_string())
    );
}

#[test]
fn parse_pgn_relation_udf_resolution() {
    let policy = parse_policy_text(
        "SINK reports CONSTRAINT max(reports.total) > 0 ON FAIL RELATION UDF abort_batch",
    )
    .expect("policy should parse");
    assert_eq!(
        policy.resolution(),
        Resolution::RelationUdf("abort_batch".to_string())
    );
}

#[test]
fn parse_pgn_rejects_llm_resolution() {
    let err = parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL LLM")
        .expect_err("LLM should be rejected");
    assert!(err.to_string().contains("invalid resolution: LLM"));
}

#[test]
fn parse_pgn_rejects_invalidate_resolution() {
    let err = parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL INVALIDATE")
        .expect_err("INVALIDATE should be rejected");
    assert!(err.to_string().contains("invalid resolution: INVALIDATE"));
}

#[test]
fn parse_pgn_preserves_description_text() {
    let policy = parse_policy_text(
        "SOURCE foo CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE DESCRIPTION keep user-facing text",
    )
    .expect("policy should parse");
    assert!(matches!(
        policy,
        PolicyIr::Pgn {
            description: Some(ref text),
            ..
        } if text == "keep user-facing text"
    ));
}

#[test]
fn parse_pgn_constraint_ignores_on_fail_in_string_literal() {
    let policy = parse_policy_text("SOURCE foo CONSTRAINT status = 'ON FAIL' ON FAIL REMOVE")
        .expect("policy should parse");
    assert_eq!(policy.constraint(), "status = 'ON FAIL'");
}

#[test]
fn parse_pgn_constraint_ignores_description_in_string_literal() {
    let policy = parse_policy_text("SOURCE foo CONSTRAINT col = 'DESCRIPTION foo' ON FAIL REMOVE")
        .expect("policy should parse");
    assert_eq!(policy.constraint(), "col = 'DESCRIPTION foo'");
}

#[test]
fn parse_pgn_dimension_commas_inside_subquery_do_not_split_list() {
    let policy = parse_policy_text(
        "SOURCE foo DIMENSION (SELECT id FROM t WHERE x IN (1, 2)) d, catalog_roles r CONSTRAINT max(foo.id) > 0 ON FAIL REMOVE",
    )
    .expect("policy should parse");
    assert!(policy.dimension_queries().contains_key("d"));
    assert_eq!(
        policy.dimension_aliases().get("r").map(String::as_str),
        Some("catalog_roles")
    );
}

#[test]
fn parse_pgn_constraint_ignores_on_fail_in_quoted_identifier() {
    let policy = parse_policy_text(r#"SOURCE foo CONSTRAINT "ON FAIL" = 1 ON FAIL REMOVE"#)
        .expect("policy should parse");
    assert_eq!(policy.constraint(), r#""ON FAIL" = 1"#);
}

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
fn parse_pgn_rejects_invalid_constraint_syntax() {
    let err = parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > ON FAIL REMOVE")
        .expect_err("policy should be invalid");

    assert!(
        err.to_string()
            .contains("invalid constraint SQL expression")
    );
}
