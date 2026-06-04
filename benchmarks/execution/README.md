# Execution benchmark scenarios

SQL fixtures for comparing rewriter output size and DuckDB execution time.
See [developer-docs/performance.md](../../developer-docs/performance.md) for the full workflow.

| File | Policy shape |
|------|----------------|
| `scan_remove.sql` | Single-table REMOVE |
| `join_remove.sql` | Join with enforcement |
| `aggregate_having.sql` | Grouped aggregate + HAVING |
| `kill_scan.sql` | KILL / `passant_kill` on scan |
| `relation_udf_insert.sql` | Relation UDF on INSERT … SELECT |

Store `EXPLAIN ANALYZE` snapshots under [explain/](explain/) when profiling a change.
