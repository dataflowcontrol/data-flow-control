//! Aggregate policy inner/outer aggregation completion tests.

use passant_core::{AggregateDfcPolicy, PolicyIr, Resolution};

use crate::common::{aggregate_policy, assert_rewrite, rewriter_with_policies};

#[test]
fn aggregate_policy_with_filter_clause_in_constraint() {
    let policy = PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["bank_txn".to_string()],
        dimensions: Vec::new(),
        sink: Some("irs_form".to_string()),
        constraint: "sum(irs_form.amount) filter (where irs_form.kind = 'Income') > 4000"
            .to_string(),
        description: None,
    });
    assert_rewrite(
        "INSERT INTO irs_form SELECT amount, kind FROM bank_txn",
        &[policy],
        "INSERT INTO irs_form SELECT amount, kind, CASE WHEN kind = 'Income' THEN amount ELSE 0 END AS __passant_agg_0 FROM bank_txn",
    );
}

#[test]
fn aggregate_policy_source_only_scan_path() {
    let policy = PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["users".to_string()],
        dimensions: Vec::new(),
        sink: None,
        constraint: "sum(users.amount) > 100".to_string(),
        description: None,
    });
    assert_rewrite(
        "SELECT id, amount FROM users",
        &[policy],
        "SELECT id, amount, sum(users.amount) AS __passant_agg_0 FROM users",
    );
}

#[test]
fn aggregate_policy_inner_group_by_with_source_aggregate() {
    let policy = aggregate_policy(
        &["foo"],
        "reports",
        "sum(reports.total) > 100 AND sum(foo.amount) > 50",
    );
    assert_rewrite(
        "INSERT INTO reports SELECT foo.category, sum(foo.amount) AS amount FROM foo GROUP BY foo.category",
        &[policy],
        "INSERT INTO reports SELECT foo.category, sum(foo.amount) AS amount, sum(foo.amount) AS __passant_agg_0, sum(reports.total) AS __passant_agg_1 FROM foo GROUP BY foo.category",
    );
}

#[test]
fn aggregate_policy_dimension_grouped_finalization() {
    let policy = PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: vec!["reports.region".to_string()],
        sink: Some("reports".to_string()),
        constraint: "sum(reports.total) > 100".to_string(),
        description: None,
    });
    let rewriter = rewriter_with_policies(&[policy]);
    let finalizations = rewriter.finalize_aggregate_queries("reports");
    assert_eq!(finalizations.len(), 1);
    assert!(finalizations[0].sql.contains("GROUP BY reports.region"));
}

#[test]
fn aggregate_policy_multi_policy_combined_valid_column() {
    let policies = vec![
        aggregate_policy(&["foo"], "reports", "sum(reports.total) > 100"),
        aggregate_policy(&["foo"], "reports", "sum(foo.amount) > 50"),
    ];
    assert_rewrite(
        "INSERT INTO reports SELECT total, amount FROM foo",
        &policies,
        "INSERT INTO reports SELECT total, amount, total AS __passant_agg_0, foo.amount AS __passant_agg_1 FROM foo",
    );
}

#[test]
fn aggregate_policy_sink_only_non_insert_finalization_only() {
    let policy = PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some("reports".to_string()),
        constraint: "sum(reports.total) > 100".to_string(),
        description: None,
    });
    let rewriter = rewriter_with_policies(&[policy]);
    let sql = rewriter
        .rewrite("SELECT id FROM foo")
        .expect("non-sink query should pass through");
    assert_eq!(sql, "SELECT id FROM foo");
    assert_eq!(rewriter.finalize_aggregate_queries("reports").len(), 1);
}

#[test]
fn aggregate_policy_invalidate_update_per_dimension() {
    let policy = PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        dimensions: vec!["reports.region".to_string()],
        sink: Some("reports".to_string()),
        constraint: "sum(reports.total) > 100".to_string(),
        description: None,
    });
    let rewriter = rewriter_with_policies(&[policy]);
    let finalizations = rewriter.finalize_aggregate_queries("reports");
    assert!(
        finalizations[0]
            .invalidate_sql
            .as_ref()
            .is_some_and(|sql| sql.contains("reports.region"))
    );
}

#[test]
fn aggregate_scan_policy_rejects_remove_resolution_at_parse() {
    use passant_core::parse_policy_text;

    let err =
        parse_policy_text("AGGREGATE SOURCE foo CONSTRAINT sum(foo.amount) > 100 ON FAIL REMOVE")
            .expect_err("aggregate policies must use INVALIDATE");
    assert!(
        err.to_string()
            .contains("aggregate policies currently only support INVALIDATE resolution")
    );
}

#[test]
fn dfc_policy_still_applies_remove_on_scan_alongside_aggregate() {
    let policies = vec![
        PolicyIr::CompatDfc {
            sources: vec!["foo".to_string()],
            required_sources: Vec::new(),
            dimensions: Vec::new(),
            sink: None,
            sink_alias: None,
            constraint: "max(foo.id) > 1".to_string(),
            on_fail: Resolution::Remove,
            description: None,
        },
        aggregate_policy(&["foo"], "reports", "sum(reports.total) > 100"),
    ];
    assert_rewrite(
        "INSERT INTO reports SELECT id FROM foo",
        &policies,
        "INSERT INTO reports SELECT id, reports.total AS __passant_agg_0 FROM foo WHERE foo.id > 1",
    );
}
