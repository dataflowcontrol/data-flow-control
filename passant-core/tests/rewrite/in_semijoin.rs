use passant_core::{PassantRewriter, Resolution, TableCatalog};

use crate::common::{pgn_policy, rewrite_with_catalog};

#[test]
fn positive_in_ungrouped_subquery_adds_group_by_for_metric() {
    let policies = vec![pgn_policy(&["lineitem"], "max(lineitem.l_quantity) >= 1")];
    let sql = rewrite(
        "SELECT o_orderkey FROM orders WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem)",
        &policies,
    );
    assert!(
        sql.contains("in_subquery") && sql.contains("GROUP BY"),
        "ungrouped IN subquery must group by join key before max metric: {sql}"
    );
    assert!(
        !sql.to_uppercase().contains(" IN (SELECT"),
        "positive IN should be converted to join: {sql}"
    );
}

#[test]
fn positive_in_distinct_subquery_adds_group_by_for_metric() {
    let policies = vec![pgn_policy(&["lineitem"], "max(lineitem.l_quantity) >= 1")];
    let sql = rewrite(
        "SELECT o_orderkey FROM orders WHERE o_orderkey IN (SELECT DISTINCT l_orderkey FROM lineitem)",
        &policies,
    );
    assert!(
        sql.contains("in_subquery") && sql.contains("GROUP BY"),
        "DISTINCT IN subquery must group by join key: {sql}"
    );
    assert!(
        !sql.contains("DISTINCT"),
        "DISTINCT should be lowered to GROUP BY for semijoin metric: {sql}"
    );
}

#[test]
fn positive_in_with_shared_source_enforces_subquery_occurrence() {
    let policies = vec![pgn_policy(&["lineitem"], "max(lineitem.l_quantity) >= 1")];
    let sql = rewrite(
        "SELECT o_orderkey FROM orders, lineitem \
         WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY l_orderkey HAVING SUM(l_quantity) > 300) \
         AND o_orderkey = l_orderkey",
        &policies,
    );
    assert!(
        sql.contains("in_subquery") && sql.contains("__passant_filter_in_metric"),
        "expected IN semijoin with hidden subquery metric: {sql}"
    );
    assert!(
        !sql.to_uppercase().contains(" IN (SELECT"),
        "positive IN should be converted to join: {sql}"
    );
}

#[test]
fn positive_in_joins_the_catalog_owner_of_an_unqualified_outer_column() {
    let policies = vec![pgn_policy(&["lineitem"], "max(lineitem.l_quantity) >= 1")];
    let mut catalog = TableCatalog::default();
    catalog.register_table("orders", vec!["o_orderkey".to_string()]);
    catalog.register_table(
        "lineitem",
        vec!["l_orderkey".to_string(), "l_quantity".to_string()],
    );

    let sql = rewrite_with_catalog(
        "SELECT o_orderkey FROM orders, lineitem \
         WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY l_orderkey HAVING SUM(l_quantity) > 300) \
         AND o_orderkey = l_orderkey",
        &policies,
        catalog,
    );

    assert_eq!(
        sql,
        "SELECT o_orderkey FROM orders JOIN (SELECT l_orderkey, max(lineitem.l_quantity) AS __passant_filter_in_metric_0 FROM lineitem GROUP BY l_orderkey HAVING SUM(l_quantity) > 300 AND max(lineitem.l_quantity) >= 1) AS in_subquery ON o_orderkey = in_subquery.l_orderkey, lineitem WHERE o_orderkey = lineitem.l_orderkey AND in_subquery.__passant_filter_in_metric_0 >= 1 AND lineitem.l_quantity >= 1"
    );
}

#[test]
fn negated_in_is_not_converted_to_join() {
    let policies = vec![pgn_policy(&["lineitem"], "max(lineitem.l_quantity) >= 1")];
    let sql = rewrite(
        "SELECT l_orderkey FROM lineitem WHERE l_orderkey NOT IN (SELECT l_orderkey FROM lineitem)",
        &policies,
    );
    assert!(sql.contains("NOT IN"), "negated IN must remain: {sql}");
}

#[test]
fn limit_wrapper_applies_in_semijoin_metric_after_limit() {
    let mut rewriter = PassantRewriter::with_catalog(crate::common::fixture_catalog(&[
        ("orders", &["o_orderkey"]),
        ("lineitem", &["l_orderkey", "l_quantity"]),
    ]));
    rewriter
        .register_policy(crate::common::pgn_policy_with(
            &["lineitem"],
            "max(lineitem.l_quantity) >= 1",
            Resolution::Remove,
        ))
        .expect("policy matches explicit fixture schema");
    let sql = rewriter
        .rewrite(
            "SELECT l_orderkey, SUM(l_quantity) AS s FROM lineitem \
             WHERE l_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY l_orderkey) \
             GROUP BY l_orderkey ORDER BY s LIMIT 5",
        )
        .expect("rewrite");
    assert!(
        sql.contains("__passant_limited") && sql.contains("in_subquery"),
        "expected limit wrapper with IN semijoin: {sql}"
    );
}

fn rewrite(sql: &str, policies: &[passant_core::PolicyIr]) -> String {
    crate::common::rewrite_with_catalog(
        sql,
        policies,
        crate::common::fixture_catalog(&[
            ("orders", &["o_orderkey"]),
            ("lineitem", &["l_orderkey", "l_quantity"]),
        ]),
    )
}
