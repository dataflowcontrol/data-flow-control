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
        "expr_1_+_foo_amount"
    );
    assert_eq!(sanitize_projection_alias(""), "expr");
    assert_eq!(sanitize_projection_alias("valid_name"), "valid_name");
}
