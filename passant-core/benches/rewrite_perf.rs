//! Criterion benchmarks for Passant rewrite performance.
//!
//! Run with: `cargo bench -p passant-core --bench rewrite_perf`

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
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

fn sink_only_policy(sink: &str) -> PolicyIr {
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

fn multi_source_policy(hot: &str, other: &str, threshold: i64) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![hot.to_string(), other.to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: format!("avg({hot}.amount) > avg({other}.amount) + {threshold}"),
        on_fail: Resolution::Remove,
        description: None,
    }
}

fn generate_policies(target_table: &str, unrelated_count: usize) -> Vec<PolicyIr> {
    let mut policies = vec![pgn_policy(target_table, 1)];
    for index in 0..unrelated_count {
        policies.push(pgn_policy(
            &format!("other_{index:06}"),
            i64::try_from(index).unwrap_or(0),
        ));
    }
    policies
}

fn generate_hot_multi_source_registry(hot: &str, multi_count: usize) -> Vec<PolicyIr> {
    let mut policies = vec![pgn_policy(hot, 1)];
    for index in 0..multi_count {
        policies.push(multi_source_policy(
            hot,
            &format!("other_{index:06}"),
            i64::try_from(index).unwrap_or(0),
        ));
    }
    policies
}

fn simple_scan_query() -> &'static str {
    "SELECT id, amount FROM orders WHERE amount > 0"
}

fn join_query() -> &'static str {
    "SELECT orders.id, customers.id FROM orders JOIN customers ON orders.id = customers.id"
}

fn partial_push_query() -> &'static str {
    "SELECT foo.id FROM foo JOIN bar ON foo.id = bar.id"
}

fn cte_chain_query() -> &'static str {
    "WITH a AS (SELECT id, amount FROM orders WHERE amount > 0), \
     b AS (SELECT id, amount FROM a WHERE amount > 1), \
     c AS (SELECT id, amount FROM b WHERE amount > 2) \
     SELECT id, amount FROM c"
}

fn register_policies(rewriter: &mut PassantRewriter, policies: &[PolicyIr]) {
    for policy in policies {
        rewriter.register_policy(policy.clone());
    }
}

fn bench_rewrite_fixed_applicable(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_fixed_applicable");
    for policy_count in [1_000_usize, 10_000, 100_000] {
        let policies = generate_policies("orders", policy_count.saturating_sub(1));
        group.bench_with_input(
            BenchmarkId::new("unrelated_policies", policy_count),
            &policies,
            |bencher, policies| {
                let mut rewriter = PassantRewriter::new();
                register_policies(&mut rewriter, policies);
                bencher.iter(|| {
                    black_box(
                        rewriter
                            .rewrite(simple_scan_query())
                            .expect("rewrite should succeed"),
                    );
                });
            },
        );
    }
    group.finish();
}

fn bench_hot_multi_source_subset(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_hot_multi_source_subset");
    for multi_count in [1_000_usize, 10_000_usize] {
        let policies = generate_hot_multi_source_registry("hot_source", multi_count);
        group.bench_with_input(
            BenchmarkId::new("multi_source_policies", multi_count),
            &policies,
            |bencher, policies| {
                let mut rewriter = PassantRewriter::new();
                register_policies(&mut rewriter, policies);
                bencher.iter(|| {
                    black_box(
                        rewriter
                            .rewrite("SELECT id, amount FROM hot_source WHERE amount > 0")
                            .expect("rewrite should succeed"),
                    );
                });
            },
        );
    }
    group.finish();
}

fn bench_sink_only_registry(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_sink_only_registry");
    let mut policies = vec![pgn_policy("orders", 1)];
    for index in 0..5_000_usize {
        policies.push(sink_only_policy(&format!("sink_{index:04}")));
    }
    group.bench_function("5000_sink_only_plain_select", |bencher| {
        let mut rewriter = PassantRewriter::new();
        register_policies(&mut rewriter, &policies);
        bencher.iter(|| {
            black_box(
                rewriter
                    .rewrite(simple_scan_query())
                    .expect("rewrite should succeed"),
            );
        });
    });
    group.finish();
}

fn bench_partial_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_partial_push");
    let policies = vec![multi_source_policy("foo", "bar", 1)];
    group.bench_function("cross_source_non_distributive", |bencher| {
        let mut rewriter = PassantRewriter::new();
        register_policies(&mut rewriter, &policies);
        bencher.iter(|| {
            black_box(
                rewriter
                    .rewrite(partial_push_query())
                    .expect("partial-push rewrite should succeed"),
            );
        });
    });
    group.finish();
}

fn bench_join_with_unrelated_policies(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_join_unrelated");
    let mut policies = generate_policies("orders", 9_999);
    policies.push(pgn_policy("customers", 2));
    group.bench_function("10k_unrelated_join", |bencher| {
        let mut rewriter = PassantRewriter::new();
        register_policies(&mut rewriter, &policies);
        bencher.iter(|| {
            black_box(
                rewriter
                    .rewrite(join_query())
                    .expect("join rewrite should succeed"),
            );
        });
    });
    group.finish();
}

fn bench_policy_registration(c: &mut Criterion) {
    let mut group = c.benchmark_group("policy_registration");
    for policy_count in [1_000_usize, 10_000, 100_000] {
        let policies = generate_policies("orders", policy_count.saturating_sub(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(policy_count),
            &policies,
            |bencher, policies| {
                bencher.iter(|| {
                    let mut rewriter = PassantRewriter::new();
                    register_policies(&mut rewriter, policies);
                    black_box(rewriter);
                });
            },
        );
    }
    group.finish();
}

fn bench_rewrite_with_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_with_stats");
    let policies = generate_policies("orders", 9_999);
    group.bench_function("10k_policies_zero_constraint_reparses", |bencher| {
        let mut rewriter = PassantRewriter::new();
        register_policies(&mut rewriter, &policies);
        let options = RewriteOptions {
            collect_stats: true,
            ..RewriteOptions::default()
        };
        bencher.iter(|| {
            black_box(
                rewriter
                    .rewrite_with_options(simple_scan_query(), options.clone())
                    .expect("rewrite should succeed"),
            );
            assert_eq!(
                rewriter
                    .last_rewrite_stats()
                    .policy_constraints_parsed_during_rewrite,
                0
            );
        });
    });
    group.finish();
}

fn ui_policy(source: &str) -> PolicyIr {
    PolicyIr::Pgn {
        sources: vec![source.to_string()],
        required_sources: Vec::new(),
        dimension_tables: Vec::new(),
        dimension_aliases: std::collections::HashMap::new(),
        dimension_queries: std::collections::HashMap::new(),
        sink: None,
        sink_alias: None,
        source_aliases: std::collections::HashMap::new(),
        constraint: format!("max({source}.amount) > 0"),
        on_fail: Resolution::Ui,
        description: None,
    }
}

fn bench_rewrite_no_policies(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_no_policies");
    group.bench_function("simple_select", |bencher| {
        let rewriter = PassantRewriter::new();
        bencher.iter(|| {
            black_box(
                rewriter
                    .rewrite(simple_scan_query())
                    .expect("rewrite should succeed"),
            );
        });
    });
    group.finish();
}

fn bench_rewrite_one_candidate(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_one_candidate");
    let policies = vec![pgn_policy("orders", 1)];
    group.bench_function("single_applicable_policy", |bencher| {
        let mut rewriter = PassantRewriter::new();
        register_policies(&mut rewriter, &policies);
        bencher.iter(|| {
            black_box(
                rewriter
                    .rewrite(simple_scan_query())
                    .expect("rewrite should succeed"),
            );
        });
    });
    group.finish();
}

fn bench_rewrite_no_candidates(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_no_candidates");
    let policies = generate_policies("other_000000", 99_999);
    group.bench_function("100k_unrelated_orders_query", |bencher| {
        let mut rewriter = PassantRewriter::new();
        register_policies(&mut rewriter, &policies);
        bencher.iter(|| {
            black_box(
                rewriter
                    .rewrite(simple_scan_query())
                    .expect("rewrite should succeed"),
            );
        });
    });
    group.finish();
}

fn bench_rewrite_100k_ui_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_100k_ui_scan");
    let policies_no_ui = generate_policies("orders", 99_999);
    group.bench_with_input(
        BenchmarkId::new("no_ui", 100_000),
        &policies_no_ui,
        |bencher, policies| {
            let mut rewriter = PassantRewriter::new();
            register_policies(&mut rewriter, policies);
            bencher.iter(|| {
                black_box(
                    rewriter
                        .rewrite(simple_scan_query())
                        .expect("rewrite should succeed"),
                );
            });
        },
    );
    let mut policies_one_ui = generate_policies("orders", 99_998);
    policies_one_ui.push(ui_policy("orders"));
    group.bench_with_input(
        BenchmarkId::new("one_ui", 100_000),
        &policies_one_ui,
        |bencher, policies| {
            let mut rewriter = PassantRewriter::new();
            register_policies(&mut rewriter, policies);
            bencher.iter(|| {
                black_box(
                    rewriter
                        .rewrite(simple_scan_query())
                        .expect("rewrite should succeed"),
                );
            });
        },
    );
    group.finish();
}

fn bench_policy_registration_compile_only(c: &mut Criterion) {
    use passant_core::policy_store::PolicyStore;
    let mut group = c.benchmark_group("policy_registration_compile_only");
    for policy_count in [1_000_usize, 10_000] {
        let policies = generate_policies("orders", policy_count.saturating_sub(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(policy_count),
            &policies,
            |bencher, policies| {
                bencher.iter(|| {
                    let mut store = PolicyStore::default();
                    for policy in policies {
                        store.register(policy.clone());
                    }
                    black_box(store);
                });
            },
        );
    }
    group.finish();
}

fn bench_rewrite_cte_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("rewrite_cte_chain");
    for policy_count in [1_000_usize, 10_000_usize] {
        let policies = generate_policies("orders", policy_count.saturating_sub(1));
        group.bench_with_input(
            BenchmarkId::new("unrelated_policies", policy_count),
            &policies,
            |bencher, policies| {
                let mut rewriter = PassantRewriter::new();
                register_policies(&mut rewriter, policies);
                bencher.iter(|| {
                    black_box(
                        rewriter
                            .rewrite(cte_chain_query())
                            .expect("rewrite should succeed"),
                    );
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_rewrite_no_policies,
    bench_rewrite_one_candidate,
    bench_rewrite_no_candidates,
    bench_rewrite_100k_ui_scan,
    bench_rewrite_fixed_applicable,
    bench_hot_multi_source_subset,
    bench_sink_only_registry,
    bench_partial_push,
    bench_join_with_unrelated_policies,
    bench_policy_registration,
    bench_policy_registration_compile_only,
    bench_rewrite_with_stats,
    bench_rewrite_cte_chain
);
criterion_main!(benches);
