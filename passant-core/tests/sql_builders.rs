use passant_core::sql::{and_exprs, binary_comparison, qualified_column};
use sqlparser::ast::{BinaryOperator, Expr};

#[test]
fn qualified_column_builds_compound_identifier() {
    let expr = qualified_column("foo", "id");
    assert_eq!(expr.to_string(), "foo.id");
}

#[test]
fn count_distinct_eq_one_builds_comparison() {
    let expr = passant_core::sql::count_distinct_eq_one("foo", "id");
    assert_eq!(expr.to_string(), "count(DISTINCT foo.id) = 1");
}

#[test]
fn scalar_subquery_wraps_expression() {
    use passant_core::sql::{function_call, qualified_column, scalar_subquery};
    let expr = scalar_subquery(
        function_call("max", vec![qualified_column("foo", "id")]),
        "foo",
    );
    assert_eq!(expr.to_string(), "(SELECT max(foo.id) FROM foo)");
}

#[test]
fn column_comparison_builds_predicate() {
    use passant_core::sql::{column_comparison, identifier};
    let expr = column_comparison("dfc", ">=", identifier("1")).expect("comparison");
    assert_eq!(expr.to_string(), "dfc >= 1");
}

#[test]
fn and_exprs_combines_predicates() {
    let left = qualified_column("foo", "id");
    let right = binary_comparison(
        qualified_column("foo", "amount"),
        BinaryOperator::Gt,
        Expr::Value(sqlparser::ast::Value::Number("1".into(), false)),
    );
    let combined = and_exprs(vec![left, right]).expect("combined");
    assert!(combined.to_string().contains("AND"));
}

#[test]
fn passant_internal_names_are_stable() {
    use passant_core::sql::passant_filter_temp_column;
    assert_eq!(
        passant_filter_temp_column("amount"),
        "__passant_filter_amount"
    );
}

#[test]
fn sanitize_projection_alias_handles_non_identifiers() {
    use passant_core::sql::sanitize_projection_alias;
    assert_eq!(
        sanitize_projection_alias("1 + foo.amount"),
        "expr_1_foo_amount"
    );
    assert_eq!(sanitize_projection_alias(""), "expr");
    assert_eq!(sanitize_projection_alias("valid_name"), "valid_name");
    assert_eq!(sanitize_projection_alias("'approve'"), "approve");
}

#[test]
fn and_exprs_preserves_boolean_precedence_on_render_and_reparse() {
    use passant_core::sql::parse_projection_expr;

    for (inputs, expected) in [
        (vec!["a OR b", "c"], "(a OR b) AND c"),
        (vec!["c", "a OR b"], "c AND (a OR b)"),
        (vec!["a OR b", "c OR d"], "(a OR b) AND (c OR d)"),
        (vec!["a AND b OR c", "d"], "(a AND b OR c) AND d"),
        (
            vec!["a OR (b AND c)", "d OR a", "b"],
            "(a OR (b AND c)) AND (d OR a) AND b",
        ),
        (vec!["a AND b", "c AND d"], "a AND b AND c AND d"),
    ] {
        let exprs = inputs
            .into_iter()
            .map(|sql| parse_projection_expr(sql).unwrap())
            .collect();
        let rendered = and_exprs(exprs).unwrap().to_string();
        assert_eq!(rendered, expected);
        assert_eq!(
            parse_projection_expr(&rendered).unwrap(),
            parse_projection_expr(expected).unwrap()
        );
    }
}

#[test]
fn and_exprs_preserves_empty_and_single_inputs() {
    use passant_core::sql::parse_projection_expr;

    assert_eq!(and_exprs(vec![]), None);
    let expr = parse_projection_expr("a OR b").unwrap();
    assert_eq!(and_exprs(vec![expr.clone()]), Some(expr));
}
