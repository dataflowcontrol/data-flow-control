use passant_core::{PassantRewriter, PolicyIr, Resolution, RewriteOptions};

fn pgn_policy(source: &str, threshold: i64) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: format!("max({source}.amount) > {threshold}"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

fn sink_only_remove_policy(sink: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: Some(sink.to_string()),
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: format!("max({sink}.amount) <= 0"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

#[test]
fn rewrite_scales_with_indexed_candidates_not_total_registry() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(pgn_policy("orders", 1));
    for index in 0..5_000_usize {
        rewriter.register_policy(pgn_policy(
            &format!("other_{index:05}"),
            i64::try_from(index).unwrap_or(0),
        ));
    }

    let options = RewriteOptions {
        collect_stats: true,
        ..RewriteOptions::default()
    };
    rewriter
        .rewrite_with_options("SELECT id, amount FROM orders WHERE amount > 0", options)
        .expect("rewrite should succeed");
    let stats = rewriter.last_rewrite_stats();
    assert_eq!(stats.policy_constraints_parsed_during_rewrite, 0);
    assert_eq!(stats.candidate_policies, 1);
    assert_eq!(stats.applicable_policies, 1);
}

#[test]
fn insert_sink_policy_lookup_uses_sink_index() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(sink_only_remove_policy("results"));
    for index in 0..2_000_usize {
        rewriter.register_policy(sink_only_remove_policy(&format!("other_sink_{index:04}")));
    }

    let rewritten = rewriter
        .rewrite("INSERT INTO results (id, amount) SELECT id, amount FROM orders")
        .expect("insert rewrite should succeed");
    assert!(rewritten.contains("WHERE"));
}

/// Manual perf smoke test:
/// `cargo test -p passant-core rewrite_1m -- --ignored --nocapture`
#[test]
#[ignore = "manual: 1M-policy registry rewrite smoke test"]
fn rewrite_1m_policies_smoke() {
    let mut rewriter = PassantRewriter::new();
    rewriter.register_policy(pgn_policy("orders", 1));
    for index in 0..1_000_000_usize {
        rewriter.register_policy(pgn_policy(
            &format!("other_{index:07}"),
            i64::try_from(index).unwrap_or(0),
        ));
    }

    let options = RewriteOptions {
        collect_stats: true,
        ..RewriteOptions::default()
    };
    let start = std::time::Instant::now();
    rewriter
        .rewrite_with_options("SELECT id, amount FROM orders WHERE amount > 0", options)
        .expect("rewrite should succeed");
    let elapsed = start.elapsed();
    let stats = rewriter.last_rewrite_stats();
    eprintln!("rewrite with 1M policies: {elapsed:?}");
    eprintln!(
        "candidate/applicable/constraint_reparses: {}/{}/{}",
        stats.candidate_policies,
        stats.applicable_policies,
        stats.policy_constraints_parsed_during_rewrite
    );
    assert_eq!(stats.policy_constraints_parsed_during_rewrite, 0);
}
