use passant_core::{PassantRewriter, Resolution};

use crate::common::{pgn_policy, rewrite};

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
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(crate::common::pgn_policy_with(
        &["lineitem"],
        "max(lineitem.l_quantity) >= 1",
        Resolution::Remove,
    ));
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
