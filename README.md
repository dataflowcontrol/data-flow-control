# Passant

Passant is a Rust-backed Data Flow Control (DFC) SQL rewrite engine with a
portable Python API (`dfc`, `Connection`, `Policy`) for DuckDB,
SQLite, PostgreSQL, ClickHouse, Apache DataFusion, and Umbra support.

The **Rust core** owns policy semantics, rewrite planning, and SQL generation.
**Python** wraps database connections, syncs catalog snapshots from adapters,
and executes rewritten SQL. Resolutions are **`REMOVE`**, **`KILL`**, tuple-level **`ON FAIL UDF`**, and
relation-level **`ON FAIL RELATION UDF`** (DuckDB-first for custom UDF resolutions; see capability table).

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
- `Policy` parsing: paper PGN syntax — `SOURCE`/`SOURCES`, `REQUIRED`, `SINK`, aliases (`SOURCE R R`),
  `DIMENSION` (table+alias or subquery), `UNIQUE`/`NOT UNIQUE`, `_OUTPUT_`, `CONSTRAINT`, `ON FAIL`,
  `DESCRIPTION`, and `PGN OVER`/`PGN UPDATE`
- `SELECT`, `INSERT ... SELECT`, `UPDATE`, and `MERGE` rewrites; **`DELETE` passthrough** (no rewrite)
- `REMOVE`, `KILL` (`passant_kill` UDF), tuple UDF, and relation-level UDF resolutions
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

# Paper PGN syntax (aliases, dimensions, UNIQUE, _OUTPUT_)
paper_policy = Policy.from_pgn("""
SOURCE Receipts R
DIMENSION catalog_users U, catalog_roles R
CONSTRAINT NOT UNIQUE Receipts.uid OR
  (S.current_user_value = U.id AND U.id = R.userid AND R.is_superuser)
ON FAIL REMOVE
""")
```

`KILL` routes through the `passant_kill()` UDF (alias of `kill()`). Rewritten SQL annotates rows in a
`t1` CTE and filters with `__passant_policy_pass OR CASE WHEN NOT __passant_policy_pass THEN passant_kill() ELSE true END`.
Tuple- and relation-level custom UDF resolutions use the `t1`–`t4` CTE pattern (see
[`docs/rewrite-pipeline.md`](docs/rewrite-pipeline.md)).

| Adapter | `exception_udf` | `tuple_udf` | `relation_udf` | Registration |
| --- | --- | --- | --- | --- |
| DuckDB | yes | yes | yes | `create_function` (session); `register_resolution_function` / `register_relation_resolution_function` |
| SQLite | yes | no | no | `sqlite3.create_function` (session) |
| DataFusion | yes | no | no | `SessionContext.register_udf` (session) |
| PostgreSQL | yes | no | no | `CREATE OR REPLACE FUNCTION` (requires `CREATE` privilege) |
| Umbra | no | no | no | `CREATE FUNCTION` not supported by Umbra yet |
| ClickHouse | yes | no | no | `CREATE OR REPLACE FUNCTION` via SQL (`throwIf`) |

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

DuckDB extension functions (e.g. Flock `llm_filter`) may appear in `CONSTRAINT` as ordinary SQL;
Passant does not integrate or special-case them. See `python/tests/test_extension_constraints.py`.

Optional Flock setup (community extension, no network during default `pytest` if already installed):

```bash
./scripts/setup_flock.sh
uv run pytest python/tests/test_extension_constraints.py -m flock
```

Rewrite/register tests run when Flock is installed. Execution tests that call `llm_filter` also need
`OPENAI_API_KEY` or `FLOCK_OPENAI_API_KEY` (DuckDB `CREATE SECRET (TYPE OPENAI, ...)`).

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
| Python API | `python/tests/test_duckdb_rewrite.py`, `test_public_api.py`, `test_vldb_2026.py`, … |
| Paper / phase tests | `test_vldb_2026.py` (paper examples); `test_paper_policy_parser.py`, `test_resolution_udf.py`, … |
| Backend conformance | `python/tests/test_backend_basic_conformance.py`, `test_backend_capabilities.py` |
