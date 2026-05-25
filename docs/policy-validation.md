# Policy Validation

Policy validation is split into syntax checks and catalog checks.

## Syntax validation (policy construction)

- Parsed by `policy.rs` when reading policy text.
- Constraint and dimension expressions must parse as SQL scalar expressions.
- Columns in constraints and dimensions must be qualified (`table.column`).
- Source and dimension lists are normalized via `normalize_policy_sources` / `normalize_policy_dimensions` (duplicate and empty name rejection).

## Catalog validation (registration time)

Rust owns semantic validation in `catalog.rs` (table/column lookups use normalized `TableKey` / `ColumnName` keys):

- source and sink tables exist
- referenced columns exist in the registered catalog snapshot
- source columns in `Policy` constraints are aggregated

Python collects catalog facts from DuckDB (`DESCRIBE`, `SHOW TABLES`) and sends a JSON snapshot to Rust via `PyPlanner.sync_catalog`. Policy dataclass constructors call Rust for constraint/dimension syntax (`validate_constraint_expression`) and list normalization (`normalize_policy_sources_py`, `normalize_policy_dimensions_py`).

## Errors

Validation failures return structured `RewriteError` variants with stable `ErrorKind` values (`UnknownTable`, `UnknownColumn`, `UnaggregatedSourceColumn`, `UnsupportedStatement`, etc.). Python raises `PassantRewriteError` (subclass of `ValueError`) with a `.kind` attribute (snake_case, matching Rust `ErrorKind::as_str()`).
