# Rewrite pipeline

## Resolution actions

Policies compile to one of:

- **Filter** (`REMOVE`) — `WHERE` / `HAVING` predicate
- **Tuple UDF** (`KILL`, `ON FAIL UDF <name>`) — CTE wrapper around the inner `SELECT`
- **Relation UDF** (`ON FAIL RELATION UDF <name>`) — relation-level abort gate (see below)
- **UI** (`ON FAIL UI`) — filter predicate with `address_violating_rows` (not the `t1`–`t4` tuple path)

## KILL (`passant_kill`)

`ON FAIL KILL` is planned as a tuple UDF named `passant_kill` (same path as `ON FAIL UDF passant_kill`).

**SELECT / INSERT … SELECT**

1. Build `t1` as the inner projection plus `__passant_policy_pass`
2. Split into passing rows (`t2`) and failing rows (`t3`)
3. Apply `passant_kill()` only on failing rows via `CASE WHEN NOT pass THEN passant_kill() …`

**UPDATE**

UPDATE cannot use the CTE tuple pipeline. KILL is applied through the same
`passant_kill` filter expression as tuple UDF resolution: rows that fail the constraint
must satisfy `pass OR CASE WHEN NOT pass THEN passant_kill() ELSE true END` in the
`WHERE` clause.

## Tuple UDF (repair and similar)

1. `t1` — annotated rows + pass column
2. `t2` — passing rows
3. `t3` — UDF on failing rows
4. `t4` — repaired rows from `t3`
5. `UNION ALL` of `t2` and `t4`

Custom tuple UDFs on `UPDATE` are not supported yet (only `passant_kill` / `KILL`).

## Relation UDF

Relation-level policies annotate the output relation with `__passant_relation_violation`,
aggregate violations with `bool_or`, and call a registered scalar UDF
`udf_name( (SELECT bool_or(__passant_relation_violation) FROM …) )`.

This supports **abort-if-any-violation** semantics (budget caps, batch gates). It does not
invoke arbitrary table-valued transforms over the full output schema; the visible columns
are preserved from the inner query.

Supported on **SELECT** and **INSERT … SELECT**. Rejected on **UPDATE** (clear error).

## Dimensions

Dimension tables are joined into the policy evaluation scope using join predicates from
the constraint when possible (inner join); otherwise a cross join is used. Dimensions
do not affect applicability (`SOURCE`/`SINK` only) and must be constrained in the
policy expression to avoid cardinality blowups.

## UNIQUE / NOT UNIQUE

- `UNIQUE T.c` → `COUNT(DISTINCT T.c) = 1`
- `NOT UNIQUE T.c` → `COUNT(DISTINCT T.c) != 1`

On **aggregated** queries, cardinality checks are evaluated in `HAVING`. On a plain scan
without grouping, `NOT UNIQUE` is false per output tuple (single-source provenance), so
policies that combine it with other predicates (for example superuser checks) behave as
documented in the paper privacy example.

Implicit uniqueness for source comparisons `T.c op value` adds
`COUNT(DISTINCT T.c) = 1 AND MIN(T.c) op value AND MAX(T.c) op value` when the column
appears in a source-table comparison.

## _OUTPUT_

Maps `_OUTPUT_.col` to insert/update/select output columns for constraint evaluation.

## UI resolution

**INSERT … SELECT** and **SELECT**

1. Add `CASE WHEN <pass> THEN TRUE ELSE address_violating_rows(...) END` to `WHERE` / `HAVING`
2. Python UDF writes corrected rows to a TSV stream (source columns then output columns)
3. `extended_duckdb` unions `filtered result UNION ALL external stream` at plan root

**UPDATE approval**

`CASE WHEN <pass> THEN TRUE ELSE passant_ui_approve(...) END` in the update `WHERE` clause.
Handler return `None` rejects the row; any dict approves without stream writes.

**UPDATE edited rows**

Same stream UDF as insert/select on failing rows, plus a follow-up
`UPDATE target SET ... FROM read_csv(stream) staged WHERE pk match` stored in
`PassantRewriter::last_ui_followup_sql()` for the Python adapter to execute.

Requires catalog unique/primary-key columns on the update target for edited mode.

## DELETE

No rewrite; statement returned as-is.
