# Python Compatibility Layer

Policy dataclass constructors delegate constraint syntax validation and source/dimension list normalization to Rust (`validate_constraint_expression_py`, `normalize_policy_sources_py`, `normalize_policy_dimensions_py`). Python still normalizes resolution enums locally.

## Python owns

- `SQLRewriter` public API and dataclasses (`DFCPolicy`, `AggregateDFCPolicy`, `PgnPolicy`)
- DuckDB connection lifecycle and UDF registration
- Catalog fact extraction (`duckdb_tables()` for enumeration, `DESCRIBE` with quoted identifiers)
- Result fetching, stream paths, context manager behavior

Insert column-list expansion (when omitted) is handled in Rust (`write_path.rs::expand_insert_columns_from_catalog`) using the synced catalog snapshot.

## Rust owns

- Policy parsing and IR (`policy.rs`)
- Catalog validation (`catalog.rs`)
- Rewrite generation (`rewriter.rs`, `full_push.rs`, `partial_push.rs`)
- Strategy selection and explain metadata (`planner.rs`, `optimizer.rs`)
- Aggregate finalization SQL

## Boundary

`passant._passant.PyPlanner` stores all policies and the catalog snapshot. `register_policy_specs` calls `register_validated_policy` in Rust after Python syncs catalog JSON.

All policy types (DFC, aggregate, PGN) are stored in Rust. Python deserializes via `dfc_policies_json()`, `aggregate_policies_json()`, and `pgn_policies_json()`. `has_registered_policies()` gates the registered rewrite path (including PGN-only registrations).

Python should not duplicate catalog semantic checks. Compatibility tests assert public behavior and error messages, not private validation helpers.

Rewrite and registration failures raise `PassantRewriteError` (a `ValueError` subclass) with a stable `.kind` string matching Rust `ErrorKind::as_str()`. Unsupported pass-through statements (e.g. `CREATE TABLE … AS SELECT`, `COPY (SELECT …)`) fail closed when policies are registered.
