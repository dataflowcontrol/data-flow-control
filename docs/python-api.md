# Passant Python API

Passant exposes a small Python layer around the Rust planner and database adapters.

## Public exports

```python
from passant import Policy, Resolution, wrap, Connection, Planner, AggregatePolicy, PgnPolicy
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

## Architecture

```text
Connection
  ├── Planner (Rust PyPlanner: policies, rewrite, explain, stats)
  └── Adapter (DuckDB: catalog introspection, execute, KILL UDF)
```

- **Rust** owns policy parsing, catalog validation, rewrite planning, and SQL generation.
- **Python** owns connection wrapping, catalog snapshots from the adapter, and execution.

`register_policy` refreshes the catalog from the adapter, validates resolution support (`KILL` requires `adapter.capabilities.supports_kill`), then registers in Rust.

## Policy types

- `Policy` — standard DFC policies (`REMOVE`, `KILL`).
- `AggregatePolicy` — aggregate policies (always `REMOVE` at registration).
- `PgnPolicy` — native PGN policy text.

Parse policy strings with `Policy.from_policy_str(...)` or `AggregatePolicy.from_policy_str(...)`.

## Resolutions

Only `Resolution.REMOVE` and `Resolution.KILL` are supported. `KILL` is implemented on DuckDB via a `kill()` scalar UDF that raises on violation.

## Errors

Rewrite failures raise `PassantRewriteError` (a `ValueError` subclass) with a stable `.kind` string from Rust.
