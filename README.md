# Data Flow Control

Data Flow Control (`data-flow-control` on PyPI) enforces policies on SQL queries.
Its Rust rewrite engine is named **Passant**, with a
portable Python API (`dfc`, `Connection`, `Policy`) for DuckDB,
SQLite, PostgreSQL, ClickHouse, Apache DataFusion, and Umbra support.

## Install

Python 3.10 or newer:

```bash
pip install data-flow-control
```

For LangChain agents, install the optional integration:

```bash
pip install "data-flow-control[langchain]"
```

Other backend extras are `postgres`, `clickhouse`, and `datafusion`.
DuckDB and SQLite work with the base installation. NumPy is included because
DuckDB requires it to register Python UDFs. Install an AWS SDK separately if your
application uses one.

The **Rust core** owns policy semantics, rewrite planning, and SQL generation.
**Python** wraps database connections, syncs catalog snapshots from adapters,
and executes rewritten SQL. Resolutions are **`REMOVE`**, **`KILL`**, **`RETRY('message')`** (see the engine matrix below), tuple-level **`ON FAIL UDF`**, and
relation-level **`ON FAIL RELATION UDF`** (DuckDB-first for custom UDF resolutions; see capability table).

For opt-in, model-neutral tool-call recovery, see the
[constraint-aware RETRY integration](developer-docs/tool-retry.md). It uses native PGN
`ON FAIL RETRY('message')`, exact failed-policy lists, and a single submitted
proposal per tool invocation—not a KILL wrapper or automatic retry loop. See the
integration document for query-shape and verification limits.

## Workspace

- `passant-core`: parser, IR, planner, optimizer, SQL rewriter, and explain output.
- `passant-cli`: CLI for rewrite, explain, plan, and policy parsing.
- `passant-py`: PyO3 extension module used by the Python package.
- `python/data_flow_control`: Python API (`dfc`, `Connection`, `Policy`, adapters).

## Supported features

`X` means the adapter implements the feature within the scope below; `—` means
unsupported. It is not a claim that every SQL shape or backend version is covered.

| Feature | DuckDB | SQLite | PostgreSQL | DataFusion | ClickHouse | Umbra |
| --- | :---: | :---: | :---: | :---: | :---: | :---: |
| SELECT with REMOVE | X | X | X | X | X | X |
| KILL | X | X | X | X | X | — |
| RETRY: exact failing-policy feedback | X | X | X | X | — | — |
| RETRY for INSERT ... SELECT | X | X | X | — | — | — |
| Tuple resolution UDF | X | — | — | — | — | — |
| Relation resolution UDF | X | — | — | — | — | — |
| UI resolution | X | — | — | — | — | — |
| Catalog-backed policy registration | X | X | X | X | X | X |

- **Read/write scope:** DataFusion RETRY is SELECT-only, matching the existing
  read-oriented KILL adapter. DuckDB, SQLite and PostgreSQL implement RETRY for
  supported SELECT / INSERT SELECT rewrite shapes; PostgreSQL live verification
  remains pending. Applicable RETRY on UPDATE,
  MERGE, DELETE or literal VALUES is not implemented. DELETE is otherwise
  passthrough, not a policy-enforced write.
- **Failure handling:** RETRY submits one SQL statement, aborts it on violation,
  and returns only the actual failing policy IDs and exact authored messages.
  It does not run the candidate twice or automatically retry it. SQLite buffers
  results before exposing a cursor so a late failure cannot leak partial rows;
  DataFusion likewise collects the result before returning it.
- **SQLite version:** RETRY requires SQLite 3.44 or newer ([`concat_ws` was added
  in 3.44](https://sqlite.org/releaselog/3_44_0.html); [MATERIALIZED CTEs in
  3.35](https://sqlite.org/releaselog/3_35_0.html)). Older runtimes reject RETRY registration
  clearly; existing KILL/REMOVE support is unchanged. Runtime tests used 3.53.1;
  this minimum is derived from required SQL features, not exhaustive version testing.
- **PostgreSQL version:** the generated guard requires PostgreSQL 12 or newer
  ([MATERIALIZED CTE syntax](https://www.postgresql.org/docs/12/release-12.html)).
  Existing backend CI targets PostgreSQL 16; live verification is still pending
  in this environment. This is a SQL-feature bound, not certification of older
  or end-of-life server releases.
- **Transactions:** use each engine's normal statement/transaction behavior;
  there is no added savepoint-isolation guarantee. PostgreSQL retains the
  existing KILL adapter's setup commits and rollback-on-error behavior, which
  can roll back earlier uncommitted work. Construction is not transaction-neutral.
  ClickHouse's KILL `X` does not imply transactional rollback for writes.
- **Verification:** deterministic RETRY runtime tests exercise DuckDB, SQLite and
  DataFusion. PostgreSQL has structured-error transport tests plus opt-in live
  integration tests requiring a PostgreSQL service; a skipped service test is
  not a live pass. ClickHouse/Umbra RETRY fail capability validation explicitly.
- **Limits:** UI requires the supported DuckDB UI extension. Custom resolution
  callbacks are currently exposed only by DuckDB. Shared query-shape limits,
  including provenance boundaries and unsupported non-distributive PartialPush
  policies using `_OUTPUT_`, are in [native RETRY](developer-docs/tool-retry.md).
  All declared SOURCE/SINK tables need real catalog entries; see
  [registration validation](developer-docs/policy-registration-catalog.md).

`Dialect` lists dialects with a registered adapter, not blanket feature parity.

## Current capabilities

- `sqlparser-rs` parser frontend and Passant-owned `QueryIr`
- rewrite optimizer with `FullPush` / `PartialPush` and explain output
- `Policy` parsing: paper PGN syntax — `SOURCE`/`SOURCES`, `REQUIRED`, `SINK`, aliases (`SOURCE R R`),
  `DIMENSION` (table+alias or subquery), `UNIQUE`/`NOT UNIQUE`, `_OUTPUT_`, `CONSTRAINT`, `ON FAIL`,
  and `DESCRIPTION`
- `SELECT`, `INSERT ... SELECT`, `UPDATE`, and `MERGE` rewrites; **`DELETE` passthrough** (no rewrite)
- `REMOVE`, `KILL` (`passant_kill` UDF), native `RETRY`, tuple UDF, and relation-level UDF resolutions
- recursive rewriting for CTEs, subqueries, and set operations
- catalog validation via adapter snapshots (not DuckDB-only)
- threshold dominance, source-set splitting, semiring join decomposition
- CLI: `rewrite`, `explain`, `plan`, `parse-policy`

Aggregate policies and invalidation (`valid` columns) are **not** supported.

## Python API

For development from a checkout (the PyPI installation needs no Rust toolchain):

```bash
uv sync --extra dev --extra langchain
uv run maturin develop --manifest-path passant-py/Cargo.toml -q
```

Basic DuckDB usage:

```python
import duckdb
from data_flow_control import Policy, Resolution, dfc

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
from data_flow_control import dfc

conn = dfc(duckdb.connect())
```

When `dialect` is omitted, `dfc` infers it from the connection type (DuckDB,
SQLite, psycopg, ClickHouse, DataFusion, or a Passant adapter). If inference
fails, pass `dialect` explicitly. Umbra requires `dfc(conn, dialect="umbra")`
for raw psycopg connections because psycopg connections are treated as Postgres.

Policy construction:

```python
from data_flow_control import Policy, Resolution

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
`t1` CTE and filters with a relation-level `MAX(CASE WHEN NOT __passant_policy_pass THEN 1 ELSE 0 END) = 1` guard that calls `passant_kill()` when any row fails, then `AND __passant_policy_pass`.
Tuple- and relation-level custom UDF resolutions use the `t1`–`t4` CTE pattern (see
[`developer-docs/rewrite-pipeline.md`](developer-docs/rewrite-pipeline.md)).

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
from data_flow_control import Policy

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
from data_flow_control import RewriteOptions

rewritten = conn.transform_query(
    "SELECT region, COUNT(*) FROM orders GROUP BY region",
    options=RewriteOptions(use_partial_push=True, collect_stats=True),
)
stats = conn.last_rewrite_stats()
summary = conn.last_statement_rewrite_summary()
```

Bulk policy registration (one catalog sync):

```python
conn.register_policies([policy_a, policy_b])
conn.refresh_catalog(force=True)  # after DDL changes
```

Performance benchmarks and budgets: [developer-docs/performance.md](developer-docs/performance.md).

Available public imports:

```python
from data_flow_control import (
    Dialect,
    PassantRewriteError,
    Policy,
    Resolution,
    RewriteOptions,
    dfc,
)
```

## Testing

From the repo root:

```bash
./scripts/install-pre-commit-hook.sh  # optional: ruff format + cargo fmt on commit
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
