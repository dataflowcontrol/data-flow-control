use passant_core::{AggregateDfcPolicy, PassantPlanner, PolicyIr, Resolution, parse_query_to_ir};

#[test]
fn lowers_select_into_query_ir() {
    let ir = parse_query_to_ir("SELECT id, max(amount) AS total FROM foo GROUP BY id")
        .expect("query should parse");
    assert!(matches!(ir, passant_core::QueryIr::Select(_)));
}

#[test]
fn planner_chooses_aggregate_inline_for_aggregate_query() {
    let ir = parse_query_to_ir("SELECT max(foo.id) FROM foo").expect("query should parse");
    let policies = vec![PolicyIr::CompatDfc {
        sources: vec!["foo".to_string()],
        sink: None,
        sink_alias: None,
        constraint: "max(foo.id) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert_eq!(
        result.chosen.strategy,
        passant_core::RewriteStrategy::AggregateInline
    );
}

#[test]
fn planner_can_defer_aggregate_policy_finalize() {
    let ir = parse_query_to_ir("INSERT INTO reports SELECT max(foo.id) AS id FROM foo")
        .expect("query should parse");
    let policies = vec![PolicyIr::CompatAggregate(AggregateDfcPolicy {
        sources: vec!["foo".to_string()],
        sink: Some("reports".to_string()),
        constraint: "sum(reports.id) > 1".to_string(),
        description: None,
    })];

    let result = PassantPlanner::new().plan_query(&ir, &policies);
    assert!(!result.chosen.finalize_metadata.is_empty());
}
