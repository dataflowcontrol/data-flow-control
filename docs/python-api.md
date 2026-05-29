# Python API

## Connection and policies

```python
from data_flow_control import Policy, Resolution, dfc

conn = dfc(duckdb.connect())
conn.register_policy(Policy.from_pgn("""
SOURCE orders
CONSTRAINT orders.amount < 1000
ON FAIL REMOVE
"""))
rows = conn.fetchall("SELECT id FROM orders")
```

## Resolutions

- `Resolution.REMOVE` — filter violating rows
- `Resolution.KILL` — tuple UDF `passant_kill` (abort when a row fails the constraint)
- `Resolution.UDF("repair_row")` — custom tuple-level UDF on `SELECT` / `INSERT … SELECT` (DuckDB)
- `Resolution.RELATION_UDF("abort_if_over_budget")` — relation-level abort gate (DuckDB)

Register custom UDFs on DuckDB:

```python
conn.register_resolution_function("repair_row", my_row_fn, ...)
conn.register_relation_resolution_function("abort_if_over_budget", my_relation_fn)
```

`KILL` and `ON FAIL UDF passant_kill` share the same implementation. On `UPDATE`, only
`passant_kill`-style abort filters are supported (not arbitrary repair UDFs).

## Relation UDF semantics

The adapter registers a scalar function that receives whether **any** row in the
annotated output relation violated the constraint (`bool_or` over
`__passant_relation_violation`). Use it to abort the statement (raise) or allow it to
continue. This is not a general “replace the output table” hook.

## Flock / LLM-backed predicates

Passant does not provide `LLM(...)`. Use DuckDB extension functions in `CONSTRAINT`, for
example Flock `llm_filter(...)`. See `passant/scripts/setup_flock.sh` and
`python/tests/test_extension_constraints.py`.

## Capabilities

Each adapter exposes `Capabilities(exception_udf, tuple_udf, relation_udf)`.
Non-capable backends reject UDF policies at registration with a clear error.

## DELETE

`DELETE` statements pass through unchanged even when policies are registered.

## Paper tests

End-to-end paper examples: `python/tests/test_vldb_2026.py` (including optional Flock and
relation-level coverage).

Phase-specific tests: `test_paper_policy_parser.py`, `test_dimensions.py`,
`test_extension_constraints.py`, `test_relation_resolution.py`, etc.
