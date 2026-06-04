# EXPLAIN ANALYZE snapshots (manual)

Store DuckDB `EXPLAIN ANALYZE` output here when profiling execution-shape changes.

Suggested naming: `{fixture}_{branch}.txt` (e.g. `scan_remove_main.txt`, `kill_scan_main.txt`).

Compare before/after when changing KILL, relation UDF, dimension join, or scalar-subquery rewrite shapes.
