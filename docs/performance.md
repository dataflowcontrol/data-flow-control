# Passant performance benchmarks

Implementation status for the full speed plan: [passant-speed-status.md](passant-speed-status.md).

## Rust Criterion

From `passant/`:

```bash
cargo bench -p passant-core --bench rewrite_perf
```

Results are written under `passant/target/criterion/`. Compare baselines across branches with:

```bash
cargo bench -p passant-core --bench rewrite_perf -- --save-baseline main
cargo bench -p passant-core --bench rewrite_perf -- --baseline main
```

### Benchmark groups

| Group | Scenario |
|-------|----------|
| `rewrite_no_policies` | Empty registry, simple SELECT |
| `rewrite_one_candidate` | Single applicable policy on `orders` |
| `rewrite_no_candidates` | 100k unrelated policies, query on `orders` only |
| `rewrite_100k_ui_scan` | 100k policies with/without one UI policy |
| `rewrite_fixed_applicable` | Applicable policy + N unrelated |
| `policy_registration` | Full `PassantRewriter::register_policy` |
| `policy_registration_compile_only` | `PolicyStore::register` only (no catalog) |
| `rewrite_with_stats` | Stats collection, zero constraint re-parses during rewrite |

## Python timing scripts

Report-only helpers under `passant/scripts/` (do not fail CI by default):

```bash
cd passant
uv run python scripts/bench_register_policy.py
uv run python scripts/bench_bulk_register.py
uv run python scripts/bench_execute.py
```

## Execution benchmarks

Rewriter latency alone does not predict DuckDB runtime. Scenario fixtures live under `passant/benchmarks/execution/`:

- `scan_remove.sql` — single-table REMOVE
- `join_remove.sql` — join with unrelated registry
- `aggregate_having.sql` — grouped query with HAVING policy

Record for each scenario:

- Rewritten SQL byte length
- Wall-clock `execute()` time (warm connection)
- Optional `EXPLAIN ANALYZE` snapshot in `passant/benchmarks/execution/explain/`

## Rewrite stats

Enable with `RewriteOptions(collect_stats=True)` or `dfc.transform_query(..., options=RewriteOptions(collect_stats=True))`.

Phase timings: parse, analysis (including `elapsed_statement_tables_ms` and
`elapsed_scope_analysis_ms`), candidate lookup, planning, rewrite, format. See
`passant_core::RewriteStats`.

Branch rewrites (set operations, nullable joins) use `PolicyStoreView` with
`BranchPolicyEntry` so split policies reuse parent interners and pre-parsed
constraint ASTs instead of re-registering the full policy registry.

## CI perf budgets (optional)

```bash
passant/scripts/check_perf_budgets.sh
```

Runs fast-path regression tests plus a Criterion smoke subset (`rewrite_no_policies`,
`rewrite_one_candidate`, `rewrite_no_candidates`). Does not fail on absolute nanosecond
thresholds; compare Criterion baselines across branches for regressions.
