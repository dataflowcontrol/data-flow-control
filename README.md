# Passant

Passant is a Rust-backed Data Flow Control rewrite engine intended to replace
`sql_rewriter` while preserving the existing Python integration surface.

## Workspace

- `passant-core`: parser, IR, planner, optimizer, SQL rewriter, and explain output.
- `passant-cli`: CLI for rewrite, explain, plan, and policy parsing.
- `passant-py`: PyO3 extension module used by the Python package.
- `python/passant`: thin Python compatibility layer.

## Current Status

The Rust core performs compatibility rewrites for the common DFC API surface.
Implemented behavior includes:

- `sqlparser-rs` as the parser frontend
- a Passant-owned `QueryIr`
- a heuristic rewrite optimizer with explain output and explicit `FullPush` and
  `PartialPush` strategy candidates (Full-Push for semiring-distributive policies,
  Partial-Push only when required by non-distributive aggregates)
- a Rust `PassantRewriter`
- PGN/compat policy parsing for `SOURCE`/`SOURCES`, `REQUIRED`, `SINK`,
  aliases, `DIMENSION`, `_OUTPUT_`, `CONSTRAINT`, `ON FAIL`, `DESCRIPTION`,
  `AGGREGATE`, and `PGN OVER`/`PGN UPDATE`
- `SELECT`, `INSERT ... SELECT`, `UPDATE`, and `MERGE` rewrites
- `UPDATE ... FROM` source policy rewrites, including source-dependent filters,
  sink aliases, `KILL`/`UDF`, and invalidation assignments/messages
- `REMOVE`, `KILL`, `INVALIDATE`, `INVALIDATE_MESSAGE`, and SQL UDF resolver
  hooks for `LLM`/`UDF`-parsed policies, including Python `Resolution.UDF`
- sink-write invalidation for `INSERT ... SELECT`, including generated
  `valid` and `invalid_string` outputs
- recursive rewriting for CTEs, derived subqueries, expression subqueries, and
  set-operation branches
- recursive rewriting for anti-semi subqueries (`NOT EXISTS` and `NOT IN`) by
  filtering the subquery input before anti evaluation
- in-place maintenance of existing `valid` and `invalid_string` projections or
  update assignments for invalidation policies
- aggregate-policy temp columns for `INSERT ... SELECT` and Rust-generated
  validation/invalidation finalization SQL
- deterministic aggregate-policy temp column assignment across multiple
  source-aggregate policies so insert and finalization rewrites agree
- aggregate-policy temp columns for grouped `INSERT ... SELECT`, including
  inner aggregate contributions for source aggregates and count contributions
- aggregate policy dimensions, including grouped finalization and per-dimension
  invalidation updates
- Rust catalog validation in `passant-core/src/catalog.rs` (Python syncs DuckDB metadata at registration)
- Python policy registration/deletion routes through stateful Rust
  `PassantRewriter` storage while preserving Python API mirror methods
- Rust-backed policy list accessors exposed through PyO3 for DFC, aggregate,
  and PGN policy storage checks
- catalog expansion for `INSERT INTO sink SELECT ...` statements that omit
  explicit sink columns
- fail-closed sink writes for missing `REQUIRED` sources
- predicate pushdown into simple `LEFT`/`RIGHT` outer join conditions for
  policies on the nullable side
- source-local predicate pushdown into simple inner-join conditions for
  FullPush-eligible SPJ queries
- alias-aware inner self-join pushdown so a source policy is applied to each
  direct occurrence of the source table
- explicit `SEMI`/`LEFT SEMI`/`RIGHT SEMI` join predicate pushdown for supported
  policies
- `ANTI`/`LEFT ANTI`/`RIGHT ANTI` support for source-local policies, including
  pre-filtering probe inputs before anti-join evaluation
- threshold dominance collapse for same-column `REMOVE` policies, including
  `=` / `!=` on `count(distinct ...)`
- correctness-first scalar-subquery fallback for aggregate-only non-distributive
  scan policies such as `avg(source.column) > threshold`, including split
  source-local fallbacks for simple multi-source `AND` predicates
- cross-source policy rewrites on outer/full/anti joins and set operations via
  row-level predicate stripping and branch-local policy splitting
  (`source_sets` module)
- semiring distributive join decomposition (`sum(a.x) + sum(b.y) > c` pushed
  into inner-join ON clauses)
- PGN UNIQUE implicit rewrite for catalog-marked unique columns
- hidden policy-column propagation for `ORDER BY`/`LIMIT` wrappers so filters
  after limiting can reference non-output source columns without changing the
  user-visible projection
- explain metadata for applicable policies and rewrite errors instead of
  silently presenting unsupported rewrites as unchanged SQL
- scope metadata that flags joins, set operations, non-monotonic constructs,
  and source-set annotation requirements
- a CLI with policy-aware `rewrite`, `explain`, `plan`, and `parse-policy`

## Testing

Passant uses a layered test story aligned with the DFC paper's correctness
claims. Rust tests focus on rewrite correctness and DuckDB execution semantics;
Python also includes TPC-H correctness regressions for the supported query set.
TPC-H performance experiments stay in `sql_rewriter/` and
`vldb_2026_big_paper_experiments/`.

### Running tests

From `passant/`:

```bash
# Full Rust workspace (core unit + integration + CLI)
cargo test --workspace

# Core only
cargo test -p passant-core

# CLI smoke tests
cargo test -p passant-cli

# Formatting and lint (matches CI)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Python compatibility tests (PyO3 + Rust catalog validation) run in CI and locally:

```bash
uv sync --extra dev
maturin develop --manifest-path passant-py/Cargo.toml --features extension-module
uv run pytest
```

### Test layout

| Layer | Location | What it covers |
| --- | --- | --- |
| Unit tests | `passant-core/src/{parser,policy,semiring,optimizer,ir,threshold,source_sets}.rs` | Pure logic: parsing, PGN, semiring classification, optimizer ranking, threshold dominance, source-set splitting |
| Rewrite integration | `passant-core/tests/rewrite/` | Exact rewritten SQL for scans, inserts, updates, joins, recursion |
| Planner integration | `passant-core/tests/planner.rs` | Strategy selection, explain metadata, fallbacks |
| Execution integration | `passant-core/tests/execution/` | Rewritten SQL executed on in-memory DuckDB |
| Paper examples | `passant-core/tests/paper_examples.rs` | TaxAgent policies, k-anonymity dominance, state-machine UPDATE |
| CLI smoke tests | `passant-cli/tests/cli.rs` | `rewrite`, `explain`, `plan`, `parse-policy` |
| Python compat | `python/tests/test_compat.py` | PyO3 bindings, catalog validation, end-to-end `SQLRewriter` |
| Python TPC-H correctness | `python/tests/test_tpch.py` | Supported TPC-H query rewrites executed against DuckDB fixtures |
| Completion gate | `passant-core/tests/completion/` | Feature-complete behavior tests (included in default `cargo test`) |

Shared helpers live in `passant-core/tests/common/`.

Python completion gate tests live in `python/tests/test_completion_gate.py`
(`pytest -m completion`).

### Paper section mapping

| Paper topic | Test module |
| --- | --- |
| PGN policy language (Section 3.4) | `tests/policy.rs`, `tests/completion/pgn.rs`, `src/policy.rs` |
| TaxAgent examples (Section 3.5) | `tests/paper_examples.rs` |
| Full-Push / Partial-Push / fallback (Section 4) | `tests/planner.rs`, `tests/rewrite/`, `tests/completion/semiring.rs` |
| Threshold dominance (Section 4.6) | `src/threshold.rs`, `tests/completion/threshold.rs` |
| Resolutions REMOVE/KILL/INVALIDATE (Section 4.5) | `tests/execution/` |
| Aggregate finalization (Section 4.5.2) | `tests/rewrite/insert.rs`, `tests/completion/aggregate_policy.rs`, `tests/execution/aggregate_finalize.rs` |
| Symmetric self-join (Section 4.7) | `tests/completion/symmetric_self_join.rs` |
| Source-set annotations | `src/source_sets.rs`, `tests/completion/source_sets.rs` |
| State-machine workload (Section 5.5) | `tests/paper_examples.rs`, `tests/execution/update.rs` |

### Explicitly Out Of Scope

- TPC-H performance benchmarking in Passant CI. Passant keeps correctness
  regressions in Python; performance experiments stay in `sql_rewriter/` and
  `vldb_2026_big_paper_experiments/`.
