use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_applies_scan_remove_policy_without_comment_stub() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
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
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
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
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) <= 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
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
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
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
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.id > 1 ORDER BY id LIMIT 1"
    );
}

#[test]
fn rewriter_filters_after_offset_for_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 2".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id OFFSET 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.id > 2 ORDER BY id OFFSET 1"
    );
}

#[test]
fn rewriter_filters_after_limit_offset_for_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 2".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 2 OFFSET 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.id > 2 ORDER BY id LIMIT 2 OFFSET 1"
    );
}

#[test]
fn rewriter_propagates_hidden_filter_column_for_limit_wrapper() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.secret) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("SELECT id FROM foo ORDER BY id LIMIT 1")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "SELECT id FROM foo WHERE foo.secret > 1 ORDER BY id LIMIT 1"
    );
}

#[test]
fn rewriter_rejects_delete_when_policies_are_registered() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let err = rewriter
        .rewrite("DELETE FROM foo WHERE id = 1")
        .expect_err("delete should be rejected");
    assert_eq!(
        err.to_string(),
        "unsupported query form: delete with registered policies"
    );
}

#[test]
fn rewriter_rewrites_except_branch_when_policies_are_registered() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
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
