use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_applies_update_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "reports.status = 'approved'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = 'approved'")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET status = 'approved' WHERE 'approved' = 'approved'"
    );
}

#[test]
fn rewriter_applies_update_sink_alias_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: Some("r".to_string()),
        source_aliases: std::collections::HashMap::new(),
        constraint: "r.status = 'approved'".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = 'approved'")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET status = 'approved' WHERE 'approved' = 'approved'"
    );
}

#[test]
fn rewriter_fails_closed_for_missing_required_source_on_update() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "reports.status = 'approved' AND max(receipts.id) > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = 'approved'")
        .expect("query should rewrite");
    assert_eq!(sql, "UPDATE reports SET status = 'approved' WHERE false");
}

#[test]
fn rewriter_applies_update_from_source_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::Pgn {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: "reports.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = foo.status FROM foo WHERE reports.id = foo.id")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET status = foo.status FROM foo WHERE reports.id = foo.id AND foo.status = 'approved' AND foo.id > 1"
    );
}
