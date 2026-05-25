# Strategy Selection

## Executable strategies

The rewrite pipeline executes strategies in priority order:

| Strategy | Engine | When |
|---|---|---|
| Full-Push | `FullPushEngine` | All policy aggregates are semiring-distributive |
| Partial-Push | `PartialPushEngine` | Non-distributive aggregates or explicit `use_partial_push` |

There is no logical fallback rewrite path. Unsupported constructs return structured errors.

## Explain-only candidates

`optimizer.rs` also ranks metadata candidates (`RootFilter`, `ProjectionPropagation`, `AggregateInline`, `CompatibilityFallback`, etc.) for explain output. These do **not** have rewrite engines unless explicitly wired into `RewritePipeline`.

## Non-monotonic queries

Queries with `EXCEPT`, outer joins, or other non-monotonic shapes use **Full-Push** with source-set splitting (`source_sets.rs`), not a separate fallback engine.

## Explain fields

- `chosen.strategy` — selected planner strategy
- `chosen.strategy_reasons` — why the winning strategy was ranked first
- `chosen.skipped_strategies` — other top candidates and their reasons
- `chosen.rewrite_error` — structured failure when rewrite fails
- `scope.policy_aggregates_distributive` — semiring analysis result
- `candidates[].reasons` — why each candidate was ranked

Tests in `tests/explain_strategy.rs` and Python `test_rewrite.py` assert strategy names for representative queries.
