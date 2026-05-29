# Policy validation

Passant validates policies in two phases:

1. **Syntax** at parse time (`Policy.from_pgn`, `parse-policy --text`)
2. **Catalog** at registration (`Connection.register_policy`) against the adapter snapshot

## Paper PGN shape

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

Install Flock locally with `passant/scripts/setup_flock.sh`. Optional execution tests require
`OPENAI_API_KEY` or `FLOCK_OPENAI_API_KEY`.

## Unsupported (by design)

- LLM resolution intrinsics
- Invalidation / `valid` columns
- Aggregate DFC policies
