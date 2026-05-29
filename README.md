# Passant

Passant is a Rust-backed Data Flow Control (DFC) SQL rewrite engine with a
portable Python API (`dfc`, `Connection`, `Policy`) for DuckDB,
SQLite, PostgreSQL, ClickHouse, Apache DataFusion, and Umbra support.

The **Rust core** owns policy semantics, rewrite planning, and SQL generation.
**Python** wraps database connections, syncs catalog snapshots from adapters,
and executes rewritten SQL. Public resolutions are **`REMOVE`** and **`KILL`**
only (`KILL` requires adapter `exception_udf` support; see table below).

## Workspace

- `passant-core`: parser, IR, planner, optimizer, SQL rewriter, and explain output.
- `passant-cli`: CLI for rewrite, explain, plan, and policy parsing.
- `passant-py`: PyO3 extension module used by the Python package.
- `python/passant`: Python API (`dfc`, `Connection`, `Policy`, adapters).

## Backend support

| Level | Meaning |
| --- | --- |
| **duckdb-full** | Primary target; broad `test_duckdb_rewrite.py` coverage |
| **basic** | Adapter exists; REMOVE scan conformance passes |
| **experimental** | Adapter exists; not in routine CI unless Docker integration is enabled |

`Dialect` lists dialects with a registered adapter.

## Current capabilities

- `sqlparser-rs` parser frontend and Passant-owned `QueryIr`
- rewrite optimizer with `FullPush` / `PartialPush` and explain output
- `Policy` parsing: `SOURCE`/`SOURCES`, `REQUIRED`, `SINK`, source/sink aliases, `DIMENSION`,
  `CONSTRAINT`, `ON FAIL`, `DESCRIPTION`, and `PGN OVER`/`PGN UPDATE`
- `SELECT`, `INSERT ... SELECT`, `UPDATE`, and `MERGE` rewrites
- `REMOVE` and `KILL` resolutions
- recursive rewriting for CTEs, subqueries, and set operations
- catalog validation via adapter snapshots (not DuckDB-only)
- threshold dominance, source-set splitting, semiring join decomposition
- CLI: `rewrite`, `explain`, `plan`, `parse-policy`

Aggregate policies, invalidation (`valid` columns), and legacy `compat` APIs are **not** supported.

## Python API

Install the extension into the local Python environment before using the package:

```bash
uv sync --extra dev
uv run maturin develop --manifest-path passant-py/Cargo.toml -q
```

Basic DuckDB usage:

```python
import duckdb
from passant import Policy, Resolution, dfc

raw = duckdb.connect()
raw.execute("CREATE TABLE orders (id INTEGER, region TEXT, amount INTEGER)")
raw.execute("INSERT INTO orders VALUES (1, 'us', 100), (2, 'eu', 200)")

conn = dfc(raw)
conn.register_policy(
    Policy(
        sources=["orders"],
        constraint="orders.region = 'us'",
        on_fail=Resolution.REMOVE,
    )
)

rows = conn.fetchall("SELECT id, amount FROM orders ORDER BY id")
assert rows == [(1, 100)]
```

Apply DFC to an existing database connection:

```python
import duckdb
from passant import dfc

conn = dfc(duckdb.connect())
```

When `dialect` is omitted, `dfc` infers it from the connection type (DuckDB,
SQLite, psycopg, ClickHouse, DataFusion, or a Passant adapter). If inference
fails, pass `dialect` explicitly. Umbra requires `dfc(conn, dialect="umbra")`
for raw psycopg connections because psycopg connections are treated as Postgres.

Policy construction:

```python
from passant import Policy, Resolution

remove_policy = Policy(
    sources=["orders"],
    required_sources=["orders"],
    dimensions=["orders.region"],
    sink=None,
    constraint="orders.amount < 1000",
    on_fail=Resolution.REMOVE,
    description="Only expose small orders",
)

kill_policy = Policy(
    sources=["orders"],
    constraint="orders.region = 'us'",
    on_fail=Resolution.KILL,
)
```

`KILL` requires adapter `exception_udf` support. Each adapter registers a `kill()`
callable (Python UDF or `CREATE FUNCTION`) that raises when a violating row is
evaluated. Rewritten SQL uses short-circuit `OR kill()` so compliant rows never
invoke `kill()`.

| Adapter | `exception_udf` | Registration |
| --- | --- | --- |
| DuckDB | yes | `create_function` (session) |
| SQLite | yes | `sqlite3` `create_function` (session) |
| DataFusion | yes | `SessionContext.register_udf` (session) |
| PostgreSQL | yes | `CREATE OR REPLACE FUNCTION` (requires `CREATE` privilege) |
| Umbra | no | `CREATE FUNCTION` not supported by Umbra yet |
| ClickHouse | yes | `CREATE OR REPLACE FUNCTION` via SQL (`throwIf`) |

SQLite reports `OperationalError: user-defined function raised exception` rather
than the Passant message text unless `sqlite3.enable_callback_tracebacks(True)` is
set (not required for Passant).

To use data flow control (DFC), write PGN policies that are enforced by the Passant rewriter:

```python
from passant import Policy

policy = Policy.from_pgn("""
SOURCE orders
CONSTRAINT orders.amount < 1000
ON FAIL REMOVE
""")

conn.register_policy(policy)
```

Query rewriting and execution:

```python
rewritten = conn.transform_query("SELECT * FROM orders")
plan = conn.explain("SELECT * FROM orders")
rows = conn.fetchall("SELECT * FROM orders")
one = conn.fetchone("SELECT COUNT(*) FROM orders")
```

Rewrite options:

```python
from passant import RewriteOptions

rewritten = conn.transform_query(
    "SELECT region, COUNT(*) FROM orders GROUP BY region",
    options=RewriteOptions(use_partial_push=True, collect_stats=True),
)
stats = conn.last_rewrite_stats()
summary = conn.last_statement_rewrite_summary()
```

Available public imports:

```python
from passant import (
    Dialect,
    PassantRewriteError,
    Policy,
    Resolution,
    RewriteOptions,
    dfc,
)
```

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
