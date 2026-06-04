# Policy validation

Passant validates policies in two phases:

1. **Syntax** at parse time (`Policy.from_pgn`, `parse-policy --text`)
2. **Catalog** at registration (`Connection.register_policy`) against the adapter snapshot

## Paper PGN shape

PGN clause structure is parsed with a `pest` grammar (`policy_pgn.pest`). The grammar
extracts raw SQL spans for `CONSTRAINT`, dimension subqueries, and `DESCRIPTION`, while
skipping over single-quoted strings, double-quoted identifiers, and balanced parentheses so
clause keywords inside SQL do not terminate spans early. Full SQL validation still uses
`sqlparser` after extraction.

```
[SOURCE[S] …] [REQUIRED …] [SINK …] [DIMENSION …] CONSTRAINT … ON FAIL … [DESCRIPTION …]
```

- **Sources / sink**: table names; optional aliases (`SOURCE Receipts R`, `SINK t AS t2`)
- **Dimensions**: table+alias (`DIMENSION catalog_users U, catalog_roles R`) or subquery dimensions
- **UNIQUE / NOT UNIQUE**: explicit uniqueness guards in `CONSTRAINT`
- **_OUTPUT_**: references to output column values in write/SELECT policies
- **ON FAIL**: `REMOVE`, `KILL`, `UDF name` (tuple-level), `RELATION UDF name`

## Resolution validation

| Resolution | Adapter requirements |
| --- | --- |
| `REMOVE` | always supported |
| `KILL` | `exception_udf` (`passant_kill` / `kill`) |
| Tuple `UDF` | `tuple_udf` (DuckDB) |
| Relation `UDF` | `relation_udf` (DuckDB); rejected on `UPDATE` initially |

## Aggregate detection

Source-column constraints must reference aggregated source columns (for example
`max(foo.amount) > 10`). Passant uses a connection-aware **aggregate registry**:

- **Static dialect fallbacks** for built-ins such as `count`, `sum`, `median`, `string_agg`,
  `list` / `array_agg`, and ClickHouse combinator suffixes (`sumIf`, `groupArrayDistinct`).
- **Adapter introspection** at catalog sync (`duckdb_functions()`, SQLite
  `PRAGMA function_list`, Postgres `pg_aggregate`, ClickHouse `system.functions`, DataFusion
  `SHOW FUNCTIONS`).
- **Explicit overrides** via `Connection.register_aggregate_function_name(...)` when
  introspection is incomplete.

Register custom UDAFs **before** `dfc(conn)` when possible, or call
`Connection.refresh_aggregate_functions()` after registering a UDAF on the connection.
Scalar functions such as `abs(foo.amount)` or `lower(foo.name)` are **not** treated as
aggregates; unqualified source columns inside them are rejected.

## Extension predicates

Function calls in `CONSTRAINT` (including DuckDB community extensions such as Flock) are
validated as ordinary SQL expressions. Passant does not load or special-case extensions.
Struct/map literals inside function arguments must not be misclassified as column references.

## UNIQUE / NOT UNIQUE

- `UNIQUE T.c` expands to `COUNT(DISTINCT T.c) = 1`.
- `NOT UNIQUE T.c` expands to `COUNT(DISTINCT T.c) != 1`.

On aggregated rewrites, these predicates are evaluated in `HAVING`. On a non-aggregated
scan, each output row has single-source provenance for `T.c`, so `NOT UNIQUE` is treated as
false and policies should combine it with other disjuncts (see the paper privacy example).

Implicit uniqueness rewrites apply to **source** column comparisons `T.c op value` (not
sink columns): `COUNT(DISTINCT T.c) = 1 AND MIN(T.c) op value AND MAX(T.c) op value`.

Paper `LLM(prompt)` style predicates map to Flock scalar functions, for example:

```sql
llm_filter(
  {'model_name': 'default'},
  {'prompt': 'Does this mention explosives?', 'context_columns': [{'data': products.description}]}
)
```

Install Flock locally with `scripts/setup_flock.sh`. Optional execution tests require
`OPENAI_API_KEY` or `FLOCK_OPENAI_API_KEY`.

## Unsupported resolutions and policy shapes

- `ON FAIL LLM` / `ON FAIL INVALIDATE` (use `REMOVE`, `KILL`, `UDF`, `RELATION UDF`, or `UI`)
- Invalidation / `valid` columns
- Aggregate DFC policies (`AGGREGATE` prefix)
