# Passant

Passant is a Rust-backed Data Flow Control rewrite engine intended to replace
`sql_rewriter` while preserving the existing Python integration surface.

## Workspace

- `passant-core`: parser, IR, planner, optimizer, SQL rewriter, and explain output.
- `passant-cli`: CLI for rewrite, explain, plan, and policy parsing.
- `passant-py`: PyO3 extension module used by the Python package.
- `python/passant`: thin Python compatibility layer.

## Current Status

The Rust core now performs real compatibility rewrites for the common DFC API
surface instead of returning comment-prefixed original SQL. Implemented behavior
includes:

- `sqlparser-rs` as the parser frontend
- a Passant-owned `QueryIr`
- a heuristic rewrite optimizer with explain output and explicit `FullPush`,
  `PartialPush`, and `LogicalFallback` strategy candidates
- semiring aggregate analysis for policy constraints, exposed through explain
  metadata and used to avoid unsafe Full-Push choices for non-distributive
  aggregates
- a Rust `PassantRewriter`
- PGN/compat policy parsing for `SOURCE`/`SOURCES`, `REQUIRED`, `SINK`,
  aliases, `DIMENSION`, `_OUTPUT_`, `CONSTRAINT`, `ON FAIL`, `DESCRIPTION`,
  and `AGGREGATE`
- `SELECT`, `INSERT ... SELECT`, and basic `UPDATE` rewrites
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
- simple aggregate-policy temp columns for `INSERT ... SELECT` and Rust-generated
  validation/invalidation finalization SQL
- deterministic aggregate-policy temp column assignment across multiple
  source-aggregate policies so insert and finalization rewrites agree
- aggregate-policy temp columns for grouped `INSERT ... SELECT`, including
  inner aggregate contributions for source aggregates and count contributions
- aggregate policy dimensions, including grouped finalization and per-dimension
  invalidation updates
- DuckDB-backed catalog validation in the Python compatibility layer
- Python policy registration/deletion routes through stateful Rust
  `PassantRewriter` storage while preserving Python API mirror methods
- Rust-backed policy list accessors exposed through PyO3 for DFC and aggregate
  policy storage checks
- catalog expansion for `INSERT INTO sink SELECT ...` statements that omit
  explicit sink columns
- fail-closed sink writes for missing `REQUIRED` sources and clear rejection of
  currently unsafe `EXCEPT` rewrites
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
- threshold dominance collapse for simple same-column `REMOVE` policies
- correctness-first scalar-subquery fallback for aggregate-only non-distributive
  scan policies such as `avg(source.column) > threshold`, including split
  source-local fallbacks for simple multi-source `AND` predicates
- source-local `FULL JOIN` enforcement by filtering base inputs before the join,
  with cross-source predicates rejected until source-set semantics are available
- clear rejection of outer-join and set-operation policy cases that require
  per-tuple source-set annotations before those annotations exist
- source-local splitting for decomposable multi-source outer-join policies whose
  top-level conjuncts each reference one source
- source-local splitting for decomposable multi-source `UNION`/`INTERSECT`
  policies whose top-level conjuncts each reference one set-operation branch
- hidden policy-column propagation for `ORDER BY`/`LIMIT` wrappers so filters
  after limiting can reference non-output source columns without changing the
  user-visible projection
- explain metadata for applicable policies and rewrite errors instead of
  silently presenting unsupported rewrites as unchanged SQL
- scope metadata that flags joins, set operations, non-monotonic constructs,
  and source-set annotation requirements
- a CLI with policy-aware `rewrite`, `explain`, `plan`, and `parse-policy`

## Testing

Passant uses a layered Rust test story aligned with the DFC paper's correctness
claims. TPC-H performance experiments stay in `sql_rewriter/` and
`vldb_2026_big_paper_experiments/`; Passant Rust tests focus on rewrite
correctness and DuckDB execution semantics.

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

Python compatibility tests (PyO3 + catalog validation) run separately:

```bash
uv sync --extra dev
maturin develop --manifest-path passant-py/Cargo.toml --features extension-module
uv run pytest
```

### Test layout

| Layer | Location | What it covers |
| --- | --- | --- |
| Unit tests | `passant-core/src/{parser,policy,semiring,optimizer,ir,threshold}.rs` | Pure logic: parsing, PGN, semiring classification, optimizer ranking, threshold dominance |
| Rewrite integration | `passant-core/tests/rewrite/` | Exact rewritten SQL for scans, inserts, updates, joins, recursion |
| Planner integration | `passant-core/tests/planner.rs` | Strategy selection, explain metadata, fallbacks |
| Execution integration | `passant-core/tests/execution/` | Rewritten SQL executed on in-memory DuckDB |
| Paper examples | `passant-core/tests/paper_examples.rs` | TaxAgent policies, k-anonymity dominance, state-machine UPDATE |
| CLI smoke tests | `passant-cli/tests/cli.rs` | `rewrite`, `explain`, `plan`, `parse-policy` |
| Python compat | `python/tests/test_compat.py` | PyO3 bindings, catalog validation, end-to-end `SQLRewriter` |
| Completion gate | `passant-core/tests/completion/` | `#[ignore]` tests that must pass before Passant is complete |

Shared helpers live in `passant-core/tests/common/`.

### Completion-gated tests

Incomplete Passant features have tests in `passant-core/tests/completion/`. Each
test is marked `#[ignore = "completion:<feature>"]` so default CI stays green
while the suite encodes the full intended behavior.

```bash
# Default: active tests only (includes conformance + policy storage)
cargo test --workspace

# Track completion progress (expect failures until features land)
cargo test --workspace -- --include-ignored

# Filter by feature label
cargo test -p passant-core --test completion -- --include-ignored --test-threads=1
```

| Label | Feature |
| --- | --- |
| `completion:source_set_annotations` | Provenance / per-tuple source-set columns |
| `completion:semiring_full_push` | Full-Push semiring inline rewrites |
| `completion:semiring_partial_push` | Partial-Push semiring rewrites |
| `completion:aggregate_policy_inner_outer` | Aggregate policy inner/outer aggregation |
| `completion:threshold_equality` | Threshold dominance for `=` / `!=` |
| `completion:pgn_unique` | PGN UNIQUE implicit rewrite |
| `completion:symmetric_self_join` | Section 4.7 symmetric self-join optimization |
| `completion:flowguard` | `FlowGuardPolicy` / `NativeFlowGuard` |
| `completion:sql_rewriter_parity` | High-value `sql_rewriter` behavior ports |

Python mirrors live in `python/tests/test_completion_gate.py` (`pytest -m completion`).

### Paper section mapping

| Paper topic | Test module |
| --- | --- |
| PGN policy language (Section 3.4) | `tests/policy.rs`, `src/policy.rs` |
| TaxAgent examples (Section 3.5) | `tests/paper_examples.rs` |
| Full-Push / Partial-Push / fallback (Section 4) | `tests/planner.rs`, `tests/rewrite/` |
| Threshold dominance (Section 4.6) | `src/threshold.rs`, `tests/rewrite/scan.rs` |
| Resolutions REMOVE/KILL/INVALIDATE (Section 4.5) | `tests/execution/` |
| Aggregate finalization (Section 4.5.2) | `tests/rewrite/insert.rs`, `tests/execution/aggregate_finalize.rs` |
| State-machine workload (Section 5.5) | `tests/paper_examples.rs`, `tests/execution/update.rs` |

### Explicitly out of scope for Rust tests

- TPC-H rewrite/performance suites (covered in Python/vldb experiments)
- Section 4.7 symmetric self-join optimization (not yet implemented)
- `FlowGuardPolicy` / PGN `UNIQUE` implicit rewrite (not yet implemented)

The following paper-level work remains incomplete:

- complete Full-Push and Partial-Push semiring rewrite algorithms beyond the
  current simple source-local predicate pushdown
- actual provenance/source-set annotation columns for per-tuple applicability
- complete aggregate-policy inner/outer aggregation support beyond simple
  temp-column extraction and sink-wide finalization
- broader full/semi/anti join implementation beyond the supported preserved-side
  and semi-join cases
