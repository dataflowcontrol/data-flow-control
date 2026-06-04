use passant_core::{PassantRewriter, PolicyIr, Resolution};

use crate::common::{pgn_policy, rewrite};

#[test]
fn derived_scan_reuses_existing_projection_alias_for_policy_metric() {
    let policies = vec![pgn_policy(&["lineitem"], "max(lineitem.l_quantity) >= 1")];
    let sql = rewrite(
        "SELECT g, SUM(v) AS total \
         FROM (SELECT lineitem.l_quantity AS q, l_quantity AS v, l_orderkey AS g FROM lineitem) AS d \
         GROUP BY g",
        &policies,
    );
    assert!(
        !sql.contains("__passant_filter_policy_0_lineitem_l_quantity"),
        "must reference existing child alias, not a missing generated column: {sql}"
    );
    assert!(
        sql.contains("max(d.q)") || sql.contains("MAX(d.q)"),
        "parent HAVING should aggregate the existing child column: {sql}"
    );
}

#[test]
fn derived_scan_under_outer_aggregate_defers_row_filter_to_parent_having() {
    let policies = vec![pgn_policy(&["lineitem"], "max(lineitem.l_quantity) >= 1")];
    let sql = rewrite(
        "SELECT g, SUM(v) FROM (SELECT l_quantity AS v, l_orderkey AS g FROM lineitem) AS d GROUP BY g",
        &policies,
    );
    assert!(
        !sql.contains("lineitem.l_quantity >= 1") && !sql.contains("l_quantity >= 1"),
        "inner scan must not row-filter policy metric: {sql}"
    );
    assert!(
        sql.to_ascii_lowercase().contains("having") && sql.contains("max("),
        "parent must enforce policy at aggregate grain: {sql}"
    );
}

#[test]
fn derived_scan_preserves_aggregate_inputs_for_passing_groups() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["lineitem".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(lineitem.l_quantity) >= 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    let sql = rewriter
        .rewrite(
            "SELECT g, SUM(v) FROM (SELECT l_quantity AS v, 1 AS g FROM lineitem) AS d GROUP BY g",
        )
        .expect("rewrite");
    assert!(
        !sql.contains("WHERE") || !sql.contains("l_quantity >= 1"),
        "must not filter inner rows: {sql}"
    );
}
