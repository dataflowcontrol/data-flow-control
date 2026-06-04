use passant_core::{
    SqlDialect, parse_policy_text, parse_query_with_dialect, sql::parse_policy_expr_duckdb,
};

#[test]
fn policy_constraint_parsing_is_independent_of_query_dialect() {
    let constraint = "max(foo.id) > 1";
    let policy_expr = parse_policy_expr_duckdb(constraint).expect("policy expr should parse");
    assert_eq!(policy_expr.to_string(), "max(foo.id) > 1");

    for dialect in [SqlDialect::DuckDb, SqlDialect::Postgres, SqlDialect::SQLite] {
        let _ =
            parse_query_with_dialect(&format!("SELECT id FROM foo WHERE {constraint}"), dialect)
                .expect("query should parse for dialect");
    }

    let policy = parse_policy_text("SOURCE foo CONSTRAINT max(foo.id) > 1 ON FAIL REMOVE")
        .expect("policy text should parse");
    assert_eq!(policy.constraint(), constraint);
}
