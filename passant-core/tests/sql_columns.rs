use passant_core::sql::{collect_qualified_columns_from_expr, parse_projection_expr};

#[test]
fn collect_qualified_columns_finds_compound_identifiers() {
    let expr = parse_projection_expr("orders.amount").expect("parse column");
    let columns = collect_qualified_columns_from_expr(&expr);
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].display_sql(), "orders.amount");
}

#[test]
fn collect_qualified_columns_walks_aggregate_arguments() {
    let expr = parse_projection_expr("max(orders.amount)").expect("parse aggregate");
    let columns = collect_qualified_columns_from_expr(&expr);
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].column.as_str(), "amount");
}
