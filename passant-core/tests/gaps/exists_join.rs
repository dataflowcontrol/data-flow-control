//! EXISTS→JOIN rewrite (TPC-H Q04-style) under Full-Push.

use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn exists_subquery_with_policy_on_inner_table_rewrites_to_join() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["lineitem".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(lineitem.l_quantity) >= 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let query = "SELECT o_orderpriority, COUNT(*) AS order_count \
FROM orders \
WHERE o_orderdate >= CAST('1993-07-01' AS DATE) \
  AND o_orderdate < CAST('1993-10-01' AS DATE) \
  AND EXISTS ( \
    SELECT * FROM lineitem \
    WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate \
  ) \
GROUP BY o_orderpriority \
ORDER BY o_orderpriority";

    let sql = rewriter.rewrite(query).expect("query should rewrite");
    assert!(sql.to_ascii_uppercase().contains("JOIN"));
    assert!(sql.to_ascii_uppercase().contains("HAVING"));
    assert!(sql.contains("exists_subquery"));
}

#[test]
fn exists_subquery_aggregation_with_inner_policy_rewrites_to_join() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Dfc {
        sources: vec!["lineitem".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(lineitem.l_quantity) >= 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let query = "SELECT o_orderkey, COUNT(*) \
FROM orders \
WHERE EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey) \
GROUP BY o_orderkey";

    let sql = rewriter.rewrite(query).expect("query should rewrite");
    assert!(sql.to_ascii_uppercase().contains("JOIN"));
    assert!(sql.to_ascii_uppercase().contains("HAVING"));
}
