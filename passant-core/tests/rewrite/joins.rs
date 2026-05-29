use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_applies_policy_to_joined_source_table() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar JOIN foo ON bar.id = foo.id")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar JOIN foo ON bar.id = foo.id AND foo.id > 1"
    );
}

#[test]
fn rewriter_applies_policy_to_each_inner_self_join_alias() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT a.id, b.id FROM foo AS a JOIN foo AS b ON a.id = b.id")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT a.id, b.id FROM foo AS a JOIN foo AS b ON a.id = b.id AND b.id > 1 WHERE a.id > 1"
    );
}

#[test]
fn rewriter_pushes_nullable_side_left_join_policy_into_join_condition() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id AND foo.id > 1"
    );
}

#[test]
fn rewriter_pushes_nullable_side_right_join_policy_into_join_condition() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM foo RIGHT JOIN bar ON bar.id = foo.id")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM foo RIGHT JOIN bar ON bar.id = foo.id AND foo.id > 1"
    );
}

#[test]
fn rewriter_rewrites_outer_join_policy_with_source_sets() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("cross-source outer join policy should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id"
    );
}

#[test]
fn rewriter_splits_source_local_outer_join_policy_that_would_need_source_sets() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > 1 AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("source-local outer join policy should split");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id AND foo.id > 1 WHERE bar.id > 1"
    );
}

#[test]
fn rewriter_rewrites_cross_source_outer_join_policy_with_source_sets() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["bar".to_string(), "foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > max(foo.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id")
        .expect("cross-source outer join policy should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar LEFT JOIN foo ON bar.id = foo.id WHERE bar.id > foo.id"
    );
}

#[test]
fn rewriter_splits_source_local_union_policy_that_would_need_source_sets() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1 AND max(bar.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo UNION ALL SELECT id FROM bar")
        .expect("source-local multi-source set operation policy should split");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.id > 1 UNION ALL SELECT id FROM bar WHERE bar.id > 1"
    );
}

#[test]
fn rewriter_splits_source_local_intersect_policy_that_would_need_source_sets() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1 AND max(bar.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo INTERSECT SELECT id FROM bar")
        .expect("source-local multi-source intersect policy should split");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.id > 1 INTERSECT SELECT id FROM bar WHERE bar.id > 1"
    );
}

#[test]
fn rewriter_passes_through_cross_source_union_all_when_branch_split_unavailable() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > max(bar.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo UNION ALL SELECT id FROM bar")
        .expect("cross-source union should pass through unchanged");
    assert_eq!(sql, "SELECT id FROM foo UNION ALL SELECT id FROM bar");
}

#[test]
fn rewriter_filters_full_join_source_before_join() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id")
        .expect("source-local full join policy should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar FULL JOIN (SELECT * FROM foo WHERE foo.id > 1) AS foo ON bar.id = foo.id"
    );
}

#[test]
fn rewriter_rewrites_cross_source_full_join_policy_with_source_sets() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > max(bar.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id")
        .expect("cross-source full join policy should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar FULL JOIN foo ON bar.id = foo.id WHERE foo.id > bar.id"
    );
}

#[test]
fn rewriter_pushes_policy_into_semi_join_condition() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar SEMI JOIN foo ON bar.id = foo.id")
        .expect("semi join should rewrite");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar SEMI JOIN foo ON bar.id = foo.id AND foo.id > 1"
    );
}

#[test]
fn rewriter_pushes_policy_into_right_semi_join_condition() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT foo.id FROM bar RIGHT SEMI JOIN foo ON bar.id = foo.id")
        .expect("right semi join should rewrite");
    assert_eq!(
        sql,
        "SELECT foo.id FROM bar RIGHT SEMI JOIN foo ON bar.id = foo.id AND bar.id > 1"
    );
}

#[test]
fn rewriter_allows_anti_join_policy_on_preserved_source() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(bar.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id")
        .expect("anti join should allow policies on preserved rows");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id WHERE bar.id > 1"
    );
}

#[test]
fn rewriter_filters_anti_join_probe_source_before_join() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT bar.id FROM bar ANTI JOIN foo ON bar.id = foo.id")
        .expect("anti join probe-side policy should filter probe source");
    assert_eq!(
        sql,
        "SELECT bar.id FROM bar ANTI JOIN (SELECT * FROM foo WHERE foo.id > 1) AS foo ON bar.id = foo.id"
    );
}
