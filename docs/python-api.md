# Passant Python API

Portable wrapper around the Rust planner and database adapters.

## Public exports

```python
from passant import (
    Policy,
    AggregatePolicy,
    PgnPolicy,
    Resolution,
    Connection,
    Planner,
    RewriteOptions,
    wrap,
    connect,
    SUPPORTED_DIALECTS,
    IMPLEMENTED_DIALECTS,
)
```

## Quick start

```python
import duckdb
from passant import Policy, Resolution, wrap

db = wrap(duckdb.connect(), dialect="duckdb")
db.execute("CREATE TABLE foo (id INTEGER)")
db.register_policy(
    Policy(
        sources=["foo"],
        constraint="max(foo.id) > 1",
        on_fail=Resolution.REMOVE,
    )
)
rows = db.fetchall("SELECT id FROM foo")
```

Or open a connection from a URL:

```python
from passant import connect, Policy, Resolution

db = connect("duckdb:///:memory:")
db = connect("sqlite:///my.db")
db = connect("datafusion://")
# db = connect("clickhouse://localhost/default")  # requires clickhouse extra + server
# db = connect("postgresql://user:pass@localhost/db")  # requires psycopg extra
```

## Architecture

```text
Connection
  ├── Planner (Rust PyPlanner)
  └── Adapter (catalog introspection, execute, KILL UDF when supported)
```

- **Rust** — policy parsing, catalog validation, rewrite planning, SQL generation, explain, stats.
- **Python** — connection wrapping, normalized catalog snapshots, execution.

`register_policy` refreshes the catalog from the adapter, validates `KILL` against `adapter.capabilities.exception_udf`, then registers in Rust.

## Dialects

`IMPLEMENTED_DIALECTS` means an adapter exists and **basic REMOVE scan conformance** passes. It does **not** guarantee dialect-correct generated SQL or full rewrite coverage on that engine.

| Dialect | Adapter | `KILL` | Conformance depth | Notes |
| --- | --- | --- | --- | --- |
| `duckdb` | Yes | Yes | Full | Primary development target; broad `test_rewrite.py` coverage |
| `sqlite` | Yes | No | REMOVE scan | In-memory integration |
| `postgres` | Yes | No | REMOVE scan | Docker on `:15432`; `information_schema` catalog |
| `clickhouse` | Yes | No | REMOVE scan | Docker on `:18123`; limited write/CTE capabilities |
| `datafusion` | Yes | No | REMOVE scan | **Query engine only** — register tables via PyArrow/`register_record_batches`, not `CREATE TABLE` via `db.execute` |
| `umbra` | Yes | No | REMOVE scan | Docker on `:15433`; `pg_catalog` introspection (no `information_schema`) |

Use `wrap(existing_conn, dialect="...")` when you already have a driver connection.
Use `connect(url)` for supported URL schemes.

## Docker integration services

Local Postgres, ClickHouse, and Umbra tests use the images already on this machine:

- `postgres:16` on port `15432`
- `clickhouse/clickhouse-server:latest` on port `18123`
- `umbradb/umbra:latest` on port `15433`

```bash
cd passant
docker compose up -d
uv sync --extra dev --extra postgres --extra clickhouse
uv run maturin develop -q
uv run pytest python/tests/ -m "postgres or clickhouse or umbra"
```

Or use `./scripts/run-integration-tests.sh`.

Set `PASSANT_POSTGRES_URL`, `PASSANT_CLICKHOUSE_URL`, or `PASSANT_UMBRA_URL` to override defaults.

```python
from passant import RewriteOptions

db.transform_query(
    "SELECT id FROM foo",
    options=RewriteOptions(use_partial_push=False, collect_stats=True, dialect="sqlite"),
)
```

`dialect` overrides the catalog snapshot dialect for parsing during a single rewrite.

Adapters emit a normalized JSON snapshot:

- `dialect`, `tables`, per-table `columns` and `types`
- optional `default_schema`, `search_path`, `nullable`

Rust stores the dialect and uses it for SQL parsing when rewriting.

## Policy types

- `Policy` — standard DFC policies (`REMOVE`, `KILL` on DuckDB only).
- `AggregatePolicy` — aggregate policies (always `REMOVE` at registration).
- `PgnPolicy` — native PGN policy text.

## Errors

Rewrite failures raise `PassantRewriteError` with a stable `.kind` string from Rust.

## Optional extras

```bash
uv sync --extra postgres
uv sync --extra clickhouse
uv sync --extra datafusion
```

Integration tests for Postgres and ClickHouse are marked and skipped unless a server is available:

```bash
uv run pytest -m "not postgres and not clickhouse and not umbra"
```
