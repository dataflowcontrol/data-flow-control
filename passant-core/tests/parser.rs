mod common;

use passant_core::parse_query_to_ir;

#[test]
fn lowers_select_into_query_ir() {
    let ir = parse_query_to_ir("SELECT id, max(amount) AS total FROM foo GROUP BY id")
        .expect("query should parse");
    assert!(matches!(ir, passant_core::QueryIr::Select(_)));
}

#[test]
fn lowers_joined_tables_into_query_ir_scope() {
    let ir = parse_query_to_ir("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("query should parse");
    let passant_core::QueryIr::Select(select) = ir else {
        panic!("expected select IR");
    };
    assert_eq!(
        select.visible_tables(),
        vec!["foo".to_string(), "bar".to_string()]
    );
}
