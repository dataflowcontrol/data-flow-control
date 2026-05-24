use passant_core::parse_query;
use sqlparser::ast::{SetExpr, Statement};

#[test]
fn insert_values_uses_values_body_not_select() {
    let statement = parse_query("INSERT INTO dest VALUES (1), (2)").expect("parse");
    let Statement::Insert(insert) = statement else {
        panic!("expected insert");
    };
    let Some(source) = insert.source.as_ref() else {
        panic!("expected source");
    };
    assert!(
        matches!(source.body.as_ref(), SetExpr::Values(_)),
        "expected VALUES body, got {:?}",
        source.body
    );
}
