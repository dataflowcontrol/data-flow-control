use passant_core::{PassantRewriter, PolicyIr, Resolution};

fn shared_constraint_policy(source: &str) -> PolicyIr {
    PolicyIr::CompatDfc {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimensions: Vec::new(),
        sink: None,
        sink_alias: None,
        constraint: "max(source.amount) > 1".to_string(),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn rewriter_exposes_store_memory_usage() {
    let mut rewriter = PassantRewriter::new();
    for index in 0..64usize {
        rewriter.register_policy(shared_constraint_policy(&format!("source_{index:03}")));
    }
    let usage = rewriter.store_memory_usage();
    assert_eq!(usage.entry_count, 64);
    assert_eq!(usage.unique_constraint_strings, 1);
    assert_eq!(usage.unique_column_keys, 1);
}

#[test]
fn registry_index_metadata_scales_linearly_with_unique_sources() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(shared_constraint_policy("orders"));
    let policy_count = 10_000usize;
    for index in 0..policy_count {
        rewriter.register_policy(shared_constraint_policy(&format!("other_{index:06}")));
    }
    let usage = rewriter.store_memory_usage();
    assert_eq!(usage.entry_count, policy_count + 1);
    assert_eq!(usage.unique_constraint_strings, 1);
    assert_eq!(usage.unique_column_keys, 1);
    // Source + enforcement indexes store one id per unique source for this workload.
    assert!(usage.source_index_estimated_bytes <= policy_count * std::mem::size_of::<usize>() * 4);
    assert!(usage.indexed_metadata_bytes() < policy_count * 128);
}

/// Manual memory smoke test:
/// `cargo test -p passant-core large_registry_memory_budget -- --ignored --nocapture`
#[test]
#[ignore = "manual: large registry memory budget smoke test"]
fn large_registry_memory_budget() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(shared_constraint_policy("orders"));
    for index in 0..100_000usize {
        rewriter.register_policy(shared_constraint_policy(&format!("other_{index:06}")));
    }
    let usage = rewriter.store_memory_usage();
    eprintln!("100k shared-constraint registry memory usage: {usage:?}");
    assert_eq!(usage.unique_constraint_strings, 1);
    assert_eq!(usage.unique_column_keys, 1);
    assert!(usage.source_index_estimated_bytes < 100_000 * std::mem::size_of::<usize>());
    assert!(usage.indexed_metadata_bytes() < 100_000 * 128);
}

/// Manual 1M-policy memory gate:
/// `cargo test -p passant-core rewrite_1m_memory_budget -- --ignored --nocapture`
#[test]
#[ignore = "manual: 1M-policy registry memory budget smoke test"]
fn rewrite_1m_memory_budget() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(shared_constraint_policy("orders"));
    for index in 0..1_000_000usize {
        rewriter.register_policy(shared_constraint_policy(&format!("other_{index:07}")));
    }
    let usage = rewriter.store_memory_usage();
    eprintln!("1M shared-constraint registry memory usage: {usage:?}");
    assert_eq!(usage.unique_constraint_strings, 1);
    assert_eq!(usage.unique_column_keys, 1);
    // Indexed metadata should stay well below naive per-policy string duplication.
    assert!(usage.indexed_metadata_bytes() < 1_000_000 * 96);
}
