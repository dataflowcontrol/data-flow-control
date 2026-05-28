# Passant

Passant is a Rust-backed Data Flow Control (DFC) SQL rewrite engine with a
portable Python API (`wrap`, `connect`, `Connection`, `Policy`) for DuckDB,
SQLite, PostgreSQL, ClickHouse, Apache DataFusion, and partial Umbra support.

The **Rust core** owns policy semantics, rewrite planning, and SQL generation.
**Python** wraps database connections, syncs catalog snapshots from adapters,
and executes rewritten SQL. Public resolutions are **`REMOVE`** and **`KILL`**
only (`KILL` requires adapter `exception_udf` support, currently DuckDB).

## Workspace

- `passant-core`: parser, IR, planner, optimizer, SQL rewriter, and explain output.
- `passant-cli`: CLI for rewrite, explain, plan, and policy parsing.
- `passant-py`: PyO3 extension module used by the Python package.
- `python/passant`: Python API (`wrap`, `connect`, `Connection`, `Policy`, adapters).

See [docs/python-api.md](docs/python-api.md) for backend support levels and capabilities.

## Backend support

| Level | Meaning |
| --- | --- |
| **duckdb-full** | Primary target; broad `test_duckdb_rewrite.py` coverage |
| **basic** | Adapter exists; REMOVE scan conformance passes |
| **experimental** | Adapter exists; not in routine CI unless Docker integration is enabled |

`IMPLEMENTED_DIALECTS` means an adapter is registered, not feature-complete SQL support on that engine.

## Current capabilities

- `sqlparser-rs` parser frontend and Passant-owned `QueryIr`
- rewrite optimizer with `FullPush` / `PartialPush` and explain output
- `Policy` parsing: `SOURCE`/`SOURCES`, `REQUIRED`, `SINK`, aliases, `DIMENSION`,
  `CONSTRAINT`, `ON FAIL`, `DESCRIPTION`, and `PGN OVER`/`PGN UPDATE`
- `SELECT`, `INSERT ... SELECT`, `UPDATE`, and `MERGE` rewrites
- `REMOVE` and `KILL` resolutions
- recursive rewriting for CTEs, subqueries, and set operations
- catalog validation via adapter snapshots (not DuckDB-only)
- threshold dominance, source-set splitting, semiring join decomposition
- CLI: `rewrite`, `explain`, `plan`, `parse-policy`

Aggregate policies, invalidation (`valid` columns), and legacy `compat` APIs are **not** supported.

## Testing

From `passant/`:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

uv sync --extra dev
uv run maturin develop --manifest-path passant-py/Cargo.toml -q
uv run ruff check .
uv run pytest
```

Optional Docker integration (Postgres, ClickHouse, Umbra):

```bash
./scripts/run-integration-tests.sh
```

### Test layout

| Layer | Location |
| --- | --- |
| Rust unit + integration | `passant-core/tests/`, `passant-core/src/**` |
| CLI | `passant-cli/tests/` |
| Python API | `python/tests/test_duckdb_rewrite.py`, `test_public_api.py`, … |
| Backend conformance | `python/tests/test_conformance.py`, `test_capabilities.py` |

Planning docs for completed work live under [docs/plans/](docs/plans/).
