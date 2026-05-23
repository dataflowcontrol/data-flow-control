use passant_core::{PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_applies_update_remove_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
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
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: Some("r".to_string()),
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
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
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
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
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

#[test]
fn rewriter_applies_update_invalidate_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved'".to_string(),
        on_fail: Resolution::Invalidate,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = 'draft'")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET status = 'draft', valid = 'draft' = 'approved'"
    );
}

#[test]
fn rewriter_applies_update_from_source_invalidate_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Invalidate,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = foo.status FROM foo WHERE reports.id = foo.id")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET status = foo.status, valid = foo.status = 'approved' AND foo.id > 1 FROM foo WHERE reports.id = foo.id"
    );
}

#[test]
fn rewriter_applies_update_from_source_invalidate_message_policy() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::InvalidateMessage,
        description: Some("bad update".to_string()),
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET status = foo.status FROM foo WHERE reports.id = foo.id")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET status = foo.status, invalid_string = CASE WHEN foo.status = 'approved' AND foo.id > 1 THEN NULL ELSE 'bad update' END FROM foo WHERE reports.id = foo.id"
    );
}

#[test]
fn rewriter_maintains_existing_update_valid_assignment() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved'".to_string(),
        on_fail: Resolution::Invalidate,
        description: None,
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET valid = false, status = 'approved'")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET valid = false AND 'approved' = 'approved', status = 'approved'"
    );
}

#[test]
fn rewriter_maintains_existing_update_invalid_string_assignment() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: Vec::new(),
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved'".to_string(),
        on_fail: Resolution::InvalidateMessage,
        description: Some("bad status".to_string()),
    });

    let sql = rewriter
        .rewrite("UPDATE reports SET invalid_string = 'prior', status = 'draft'")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "UPDATE reports SET invalid_string = CASE WHEN 'draft' = 'approved' THEN 'prior' ELSE COALESCE('prior' || '; ', '') || 'bad status' END, status = 'draft'"
    );
}
