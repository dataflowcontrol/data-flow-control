use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_applies_scan_remove_policy_without_comment_stub() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("query should rewrite");
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id > 1");
}

#[test]
fn rewriter_collapses_dominated_remove_thresholds() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("query should rewrite");
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id > 10");
}

#[test]
fn rewriter_collapses_dominated_upper_thresholds() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) <= 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) < 5".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("query should rewrite");
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id < 5");
}

#[test]
fn rewriter_applies_aliases_and_having_for_aggregation() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.amount) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT f.category, sum(f.amount) FROM foo AS f GROUP BY f.category")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT f.category, sum(f.amount) FROM foo AS f GROUP BY f.category HAVING max(f.amount) > 1"
    );
}

#[test]
fn rewriter_filters_after_limit_for_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "WITH __passant_limited AS (SELECT id FROM foo ORDER BY id LIMIT 1) SELECT id FROM __passant_limited WHERE id > 1"
    );
}

#[test]
fn rewriter_filters_after_offset_for_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 2".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id OFFSET 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "WITH __passant_limited AS (SELECT id FROM foo ORDER BY id OFFSET 1) SELECT id FROM __passant_limited WHERE id > 2"
    );
}

#[test]
fn rewriter_filters_after_limit_offset_for_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 2".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 2 OFFSET 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "WITH __passant_limited AS (SELECT id FROM foo ORDER BY id LIMIT 2 OFFSET 1) SELECT id FROM __passant_limited WHERE id > 2"
    );
}

#[test]
fn rewriter_limit_without_order_by_uses_cte_wrapper() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo LIMIT 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "WITH __passant_limited AS (SELECT id FROM foo LIMIT 1) SELECT id FROM __passant_limited WHERE id > 1"
    );
}

#[test]
fn rewriter_order_by_without_limit_keeps_inline_remove() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id")
        .expect("query should rewrite");
    assert_eq!(sql, "SELECT id FROM foo WHERE foo.id > 1 ORDER BY id");
}

#[test]
fn select_kill_order_by_remapped() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Kill,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY foo.id")
        .expect("query should rewrite");
    assert!(sql.contains("passant_kill"), "expected kill wrap: {sql}");
    assert!(
        sql.contains("ORDER BY id"),
        "expected unqualified ORDER BY: {sql}"
    );
    assert!(
        !sql.contains("ORDER BY foo."),
        "qualified ORDER BY should be remapped: {sql}"
    );
}

#[test]
fn rewriter_propagates_hidden_filter_column_for_limit_wrapper() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.secret) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "WITH __passant_limited AS (SELECT id, foo.secret AS __passant_filter_secret FROM foo ORDER BY id LIMIT 1) SELECT id FROM __passant_limited WHERE __passant_filter_secret > 1"
    );
}

#[test]
fn rewriter_wraps_aggregate_limit_for_kill_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.amount) > 1".to_string(),
        on_fail: Resolution::Kill,
        description: None,
    });

    let sql = rewriter
        .rewrite(
            "SELECT category, sum(amount) FROM foo GROUP BY category ORDER BY category LIMIT 1",
        )
        .expect("query should rewrite");
    assert!(sql.contains("WITH __passant_limited AS"));
    assert!(sql.contains("__passant_filter_agg_"));
    assert!(sql.contains("passant_kill"));
    assert!(sql.contains("GROUP BY category ORDER BY category LIMIT 1"));
}

#[test]
fn rewriter_wraps_aggregate_limit_for_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.amount) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite(
            "SELECT category, sum(amount) FROM foo GROUP BY category ORDER BY category LIMIT 1",
        )
        .expect("query should rewrite");
    assert!(sql.contains("WITH __passant_limited AS"));
    assert!(sql.contains("__passant_filter_agg_"));
    assert!(sql.contains("GROUP BY category ORDER BY category LIMIT 1"));
}

#[test]
fn rewriter_wraps_limit_for_kill_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Kill,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 1")
        .expect("query should rewrite");
    assert!(sql.contains("WITH __passant_limited AS"));
    assert!(sql.contains("FROM __passant_limited"));
}

#[test]
fn rewriter_passthrough_delete_when_policies_are_registered() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("DELETE FROM foo WHERE id = 1")
        .expect("delete should pass through unchanged");
    assert_eq!(sql, "DELETE FROM foo WHERE id = 1");
}

#[test]
fn rewriter_rewrites_except_branch_when_policies_are_registered() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM bar EXCEPT SELECT id FROM foo")
        .expect("except branch rewrite should succeed");
    assert!(sql.contains("EXCEPT"));
    assert!(sql.contains("foo.id > 1"));
}
