# Passant Rewrite Pipeline

Passant rewrites SQL in a fixed pipeline:

1. **Parse** — `parser.rs` lowers SQL into `QueryIr` and a `sqlparser` AST.
2. **Validate** — `catalog.rs` validates policies against DuckDB catalog facts at registration time.
3. **Analyze** — `planner.rs` and `semiring.rs` compute scope, applicability, and aggregate distributivity.
4. **Select strategy** — `optimizer.rs` ranks candidates; `rewrite_strategy.rs` dispatches `FullPushEngine` then `PartialPushEngine`.
5. **Rewrite** — engines mutate or split SQL (`full_push.rs`, `partial_push.rs`, `rewriter/` write paths).
6. **Format** — `Statement::to_string()` at the output boundary.

## `rewriter/` modules

| Module | Role |
|--------|------|
| `mod.rs` | Orchestration: public API, policy registration, pipeline dispatch |
| `types.rs` | `PassantRewriter`, `RewriteOptions`, `RewriteContext`, `FinalizeQuery` |
| `scope.rs` | `TableScope` (visible tables and alias map) |
| `expr.rs` | Expression parsing, resolution, and kill helpers |
| `columns.rs` | Column/sink replacement and qualification |
| `aggregates.rs` | Constraint aggregates, temp columns, scan transforms, finalize SQL |
| `policy_expr.rs` | Policy applicability, filter building, join pushdown |
| `projection.rs` | Projection aliases, aggregation detection, GROUP BY join specs |
| `select.rs` | SELECT rewrite path, join pushdown, subqueries |
| `write_path.rs` | INSERT/UPDATE/MERGE rewrites |
| `exists.rs` | EXISTS/IN subquery → join rewrites |
| `helpers.rs` | Shared helpers (flatten_and, dominated policy pruning, etc.) |
| `finalize.rs` | Aggregate finalization |

`identifiers.rs` (crate root) provides typed table/column names (`TableName`, `TableKey`, `QualifiedColumn`, `SourceName`, `SinkName`, `AliasByBase`, `PolicyId`).

AST-backed SQL construction lives in `sql/builders.rs` and `sql/expr.rs`. Passant-generated column aliases use `passant_agg_temp_column`, `passant_filter_temp_column`, and `sanitize_projection_alias`. Rewriter modules use typed identifiers for sink replacement and qualified-column parsing.

Explain output (`explain.rs`) records scope metadata, the chosen strategy, and rewrite errors for unsupported paths (e.g. DELETE with registered policies).

## Extension points

- New **policy resolution**: add to `policy.rs`, filter builders in `rewriter/`, tests in `tests/rewrite/`.
- New **rewrite strategy**: implement `RewriteEngine`, register in `RewritePipeline`, document in `strategy-selection.md`.
- New **catalog rule**: add to `catalog.rs::validate_policy`, mirror with Rust unit tests and Python API tests.
