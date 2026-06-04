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
- `Resolution.UI` — interactive row repair via DuckDB `address_violating_rows` + `extended_duckdb` stream (DuckDB only)

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
example Flock `llm_filter(...)`. See `scripts/setup_flock.sh` and
`python/tests/test_extension_constraints.py`.

## UI resolution (`ON FAIL UI`)

DuckDB-only. Configure before registering UI policies:

```python
def handler(event: UiViolationEvent) -> dict[str, object] | None:
    if event.row.get("category") == "meal":
        return {"business_use_pct": 40}  # corrected output columns
    return None  # reject row

conn.configure_ui_resolution(
    handler,
    stream_endpoint="/tmp/passant-ui.tsv",
    extension_path="/path/to/external.duckdb_extension",
    update_mode="approval_only",  # or "edited_rows" for UPDATE
)
conn.register_policy(Policy(..., on_fail=Resolution.UI, sink="irs_form", ...))
```

`configure_ui_resolution()` registers UI UDFs without the extension for rewrite-only workflows.
`execute()` on INSERT/SELECT that use `address_violating_rows` requires the `external`
extension (pass `extension_path` or `LOAD` it before configure) so corrected rows are unioned back.

- **INSERT … SELECT** / **SELECT**: failing rows call the handler; corrected rows are written to a TSV stream and unioned back by `extended_duckdb`.
- **UPDATE `approval_only`**: `passant_ui_approve(...)` — handler returns non-`None` to allow the update on failing rows.
- **UPDATE `edited_rows`**: stream corrected values; Passant emits a follow-up `UPDATE ... FROM read_csv(...)` executed after the main statement.

Set `RewriteOptions(ui_stream_endpoint=...)` when transforming without `configure_ui_resolution`.

## Capabilities

Each adapter exposes `Capabilities(exception_udf, tuple_udf, relation_udf, ui_resolution)`.
Non-capable backends reject UDF policies at registration with a clear error.

## DELETE

`DELETE` statements pass through unchanged even when policies are registered.

## Paper tests

End-to-end paper examples: `python/tests/test_vldb_2026.py` (including optional Flock and
relation-level coverage).

Phase-specific tests: `test_paper_policy_parser.py`, `test_dimensions.py`,
`test_extension_constraints.py`, `test_relation_resolution.py`, etc.
