//! CI-safe regressions for indexed candidate lookup and rewrite candidate counts.

use std::collections::HashSet;

use passant_core::{
    MultiSourceLookupMode, PassantRewriter, PolicyIr, PolicyStore, Resolution, RewriteOptions,
    TableKey,
};

fn dfc(source: &str, threshold: i64) -> PolicyIr {
    PolicyIr::Dfc {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: format!("max({source}.amount) > {threshold}"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

fn sink_only_policy(sink: &str) -> PolicyIr {
    PolicyIr::Dfc {
        sources: vec![],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: Some(sink.to_string()),
        sink_alias: None,
        constraint: format!("max({sink}.amount) <= 0"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

fn multi_source_policy(hot: &str, other: &str, threshold: i64) -> PolicyIr {
    PolicyIr::Dfc {
        sources: vec![hot.to_string(), other.to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: format!("max({hot}.amount) + max({other}.amount) > {threshold}"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

fn rewrite_candidate_count(rewriter: &PassantRewriter, sql: &str) -> usize {
    let options = RewriteOptions {
        collect_stats: true,
        ..RewriteOptions::default()
    };
    rewriter
        .rewrite_with_options(sql, options)
        .expect("rewrite should succeed");
    rewriter.last_rewrite_stats().candidate_policies
}

#[test]
fn many_unrelated_single_source_policies_keep_candidate_count_small() {
    let mut store = PolicyStore::default();
    store.register(dfc("orders", 1));
    for index in 0..2_000_usize {
        store.register(dfc(
            &format!("other_{index:05}"),
            i64::try_from(index).unwrap_or(0),
        ));
    }

    let orders_only = HashSet::from([TableKey::new("orders")]);
    assert_eq!(store.candidate_ids_for_scope(&orders_only, None), vec![0]);
    store.assert_candidates_match_slow_scan(&orders_only, None, MultiSourceLookupMode::Subset);

    let mut rewriter = PassantRewriter::new();
    for policy in store.policies_vec() {
        rewriter.register_policy(policy);
    }
    assert_eq!(
        rewrite_candidate_count(&rewriter, "SELECT id, amount FROM orders WHERE amount > 0"),
        1
    );
}

#[test]
fn sink_only_policies_are_excluded_from_plain_select_candidates() {
    let mut store = PolicyStore::default();
    store.register(dfc("orders", 1));
    for index in 0..500_usize {
        store.register(sink_only_policy(&format!("sink_{index:03}")));
    }

    let orders_only = HashSet::from([TableKey::new("orders")]);
    assert_eq!(store.candidate_ids_for_scope(&orders_only, None), vec![0]);

    let mut rewriter = PassantRewriter::new();
    for policy in store.policies_vec() {
        rewriter.register_policy(policy);
    }
    assert_eq!(
        rewrite_candidate_count(&rewriter, "SELECT id, amount FROM orders WHERE amount > 0"),
        1
    );
}

#[test]
fn hot_source_multi_source_subset_lookup_avoids_policy_blowup() {
    let mut store = PolicyStore::default();
    store.register(dfc("hot_source", 1));
    for index in 0..1_000_usize {
        store.register(multi_source_policy(
            "hot_source",
            &format!("other_{index:04}"),
            i64::try_from(index).unwrap_or(0),
        ));
    }

    let hot_only = HashSet::from([TableKey::new("hot_source")]);
    assert_eq!(store.candidate_ids_for_scope(&hot_only, None), vec![0]);
    assert_eq!(
        store
            .candidate_ids_for_scope_with_overlap(&hot_only, None)
            .len(),
        1_001
    );
    store.assert_candidates_match_slow_scan(&hot_only, None, MultiSourceLookupMode::Subset);

    let mut rewriter = PassantRewriter::new();
    for policy in store.policies_vec() {
        rewriter.register_policy(policy);
    }
    assert_eq!(
        rewrite_candidate_count(
            &rewriter,
            "SELECT id, amount FROM hot_source WHERE amount > 0"
        ),
        1
    );
}

#[test]
fn partial_push_enforcement_lookup_uses_overlap_for_multi_source() {
    let mut store = PolicyStore::default();
    store.register(PolicyIr::Dfc {
        sources: vec!["foo".to_string(), "bar".to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "avg(foo.id) > avg(bar.id)".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    });

    let foo_only = HashSet::from([TableKey::new("foo")]);
    assert!(
        store
            .enforcement_candidate_ids_for_tables(&foo_only)
            .contains(&0)
    );
    assert_eq!(
        store.candidate_ids_for_scope(&foo_only, None),
        Vec::<usize>::new()
    );

    let mut rewriter = PassantRewriter::new();
    for policy in store.policies_vec() {
        rewriter.register_policy(policy);
    }
    let sql = rewriter
        .rewrite("SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id")
        .expect("partial-push rewrite should succeed");
    assert!(sql.contains("WITH base_query AS ("));
}
