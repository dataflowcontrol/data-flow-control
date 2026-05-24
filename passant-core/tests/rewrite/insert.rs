use passant_core::{AggregateDfcPolicy, PassantRewriter, PolicyIr, Resolution};

#[test]
fn rewriter_maps_insert_sink_columns_to_select_outputs() {
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
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_maps_insert_sink_alias_columns_to_select_outputs() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: Some("r".to_string()),
        constraint: "r.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_maps_output_marker_columns_to_insert_outputs() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "_OUTPUT_.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo WHERE foo.status = 'approved' AND foo.id > 1"
    );
}

#[test]
fn rewriter_fails_closed_for_missing_required_source_on_insert() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.id > 0 AND max(receipts.id) > 0".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id) SELECT other.id FROM other")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id) SELECT other.id FROM other WHERE false"
    );
}

#[test]
fn rewriter_enforces_required_source_normally_when_present_on_insert() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["receipts".to_string()],
        required_sources: vec!["receipts".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.id > 0 AND max(receipts.id) > 10".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id) SELECT receipts.id FROM receipts")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id) SELECT receipts.id FROM receipts WHERE receipts.id > 0 AND receipts.id > 10"
    );
}

#[test]
fn rewriter_maps_insert_invalidate_policy_to_valid_output() {
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
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status, valid) SELECT foo.id, foo.status, foo.status = 'approved' AND foo.id > 1 AS valid FROM foo"
    );
}

#[test]
fn rewriter_maps_insert_invalidate_message_policy_to_message_output() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        sink_alias: None,
        constraint: "reports.status = 'approved' AND max(foo.id) > 1".to_string(),
        on_fail: Resolution::InvalidateMessage,
        description: Some("bad row".to_string()),
    });

    let sql = rewriter
        .rewrite("INSERT INTO reports (id, status) SELECT foo.id, foo.status FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (id, status, invalid_string) SELECT foo.id, foo.status, CASE WHEN foo.status = 'approved' AND foo.id > 1 THEN NULL ELSE 'bad row' END AS invalid_string FROM foo"
    );
}

#[test]
fn rewriter_generates_aggregate_finalization_queries() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        constraint: "sum(reports.total) > 100".to_string(),
        description: Some("cap total".to_string()),
    }));

    let queries = rewriter.finalize_aggregate_queries("reports");
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].policy_id, "aggregate::sum(reports.total) > 100");
    assert_eq!(
        queries[0].sql,
        "SELECT (sum(reports.total) > 100) AS constraint_result FROM reports"
    );
    assert_eq!(
        queries[0].invalidate_sql.as_deref(),
        Some(
            "UPDATE reports SET valid = COALESCE(valid, true) AND (SELECT (sum(reports.total) > 100) FROM reports)"
        )
    );
}

#[test]
fn rewriter_generates_dimensioned_aggregate_finalization_queries() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: Vec::new(),
        dimensions: vec!["reports.region".to_string()],
        sink: Some("reports".to_string()),
        constraint: "sum(reports.total) > 100".to_string(),
        description: None,
    }));

    let queries = rewriter.finalize_aggregate_queries("reports");
    assert_eq!(
        queries[0].sql,
        "SELECT reports.region, (sum(reports.total) > 100) AS constraint_result FROM reports GROUP BY reports.region"
    );
    assert_eq!(
        queries[0].invalidate_sql.as_deref(),
        Some(
            "UPDATE reports SET valid = COALESCE(valid, true) AND COALESCE((SELECT (sum(__passant_group.total) > 100) FROM reports AS __passant_group WHERE __passant_group.region = reports.region), true)"
        )
    );
}

#[test]
fn rewriter_adds_simple_aggregate_policy_temp_columns_to_insert() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        constraint: "sum(foo.amount) > sum(reports.total)".to_string(),
        description: None,
    }));

    let sql = rewriter
        .rewrite("INSERT INTO reports (total) SELECT foo.amount FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (total, __passant_agg_0, __passant_agg_1) SELECT foo.amount, foo.amount AS __passant_agg_0, foo.amount AS __passant_agg_1 FROM foo"
    );

    let queries = rewriter.finalize_aggregate_queries("reports");
    assert_eq!(
        queries[0].sql,
        "SELECT (sum(__passant_agg_0) > sum(reports.total)) AS constraint_result FROM reports"
    );
}

#[test]
fn rewriter_adds_grouped_aggregate_policy_temp_columns_to_insert() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: vec!["reports.region".to_string()],
        sink: Some("reports".to_string()),
        constraint: "sum(foo.amount) >= sum(reports.total)".to_string(),
        description: None,
    }));

    let sql = rewriter
        .rewrite(
            "INSERT INTO reports (region, total) SELECT foo.region, sum(foo.amount) FROM foo GROUP BY foo.region",
        )
        .expect("grouped insert should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (region, total, __passant_agg_0, __passant_agg_1) SELECT foo.region, sum(foo.amount), sum(foo.amount) AS __passant_agg_0, sum(foo.amount) AS __passant_agg_1 FROM foo GROUP BY foo.region"
    );

    let queries = rewriter.finalize_aggregate_queries("reports");
    assert_eq!(
        queries[0].sql,
        "SELECT reports.region, (sum(__passant_agg_0) >= sum(reports.total)) AS constraint_result FROM reports GROUP BY reports.region"
    );
}

#[test]
fn rewriter_uses_count_contributions_for_aggregate_policy_temp_columns() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        constraint: "count(foo.id) >= sum(reports.total)".to_string(),
        description: None,
    }));

    let sql = rewriter
        .rewrite("INSERT INTO reports (total) SELECT foo.total FROM foo")
        .expect("count temp insert should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (total, __passant_agg_0, __passant_agg_1) SELECT foo.total, foo.id AS __passant_agg_0, foo.total AS __passant_agg_1 FROM foo"
    );

    let queries = rewriter.finalize_aggregate_queries("reports");
    assert_eq!(
        queries[0].sql,
        "SELECT (sum(__passant_agg_0) >= sum(reports.total)) AS constraint_result FROM reports"
    );
}

#[test]
fn rewriter_uses_consistent_aggregate_temp_columns_across_policies() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        constraint: "sum(foo.amount) > sum(reports.total)".to_string(),
        description: None,
    }));
    rewriter.register_policy(PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        constraint: "sum(foo.tax) > sum(reports.total)".to_string(),
        description: None,
    }));

    let sql = rewriter
        .rewrite("INSERT INTO reports (total) SELECT foo.amount FROM foo")
        .expect("query should rewrite");
    assert_eq!(
        sql,
        "INSERT INTO reports (total, __passant_agg_0, __passant_agg_1, __passant_agg_2) SELECT foo.amount, foo.amount AS __passant_agg_0, foo.amount AS __passant_agg_1, foo.tax AS __passant_agg_2 FROM foo"
    );

    let queries = rewriter.finalize_aggregate_queries("reports");
    assert_eq!(
        queries[0].sql,
        "SELECT (sum(__passant_agg_0) > sum(reports.total)) AS constraint_result FROM reports"
    );
    assert_eq!(
        queries[1].sql,
        "SELECT (sum(__passant_agg_2) > sum(reports.total)) AS constraint_result FROM reports"
    );
}
