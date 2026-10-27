# Changelog

## 0.2.1

- Preserve KILL aborts for constant-false candidate policies instead of allowing
  optimizer filtering to silently return no rows. Genuine empty candidates and
  legacy NULL filtering retain their existing behavior.
- Bind source, sink, and `_OUTPUT_` references inside `IS NULL` and `IS NOT NULL`
  predicates, including INSERT output expressions.

## 0.2.0

### Native RETRY and resolution placement

- PGN supports `ON FAIL RETRY('exact feedback')`. Python raises `RetryViolation`
  containing the actual failing policies' stable IDs and authored messages.
- RETRY executes one SQL statement and aborts on failure; it does not preflight,
  replay SQL, or automatically invoke a model to repair it.
- Non-splittable resolutions share an effect stage, ordered KILL, REMOVE, RETRY,
  then UDF. UDF failures are determined once; selected repairs compose in UDF
  definition-registration order. UDF registration accepts splittability metadata.
- RETRY is implemented for DuckDB, SQLite, PostgreSQL, and DataFusion within the
  documented query-shape and write limits. See the README Supported matrix.

### Migration from 0.1.x

- Every policy SOURCE and SINK needs an explicit catalog entry. Physical policy
  columns are validated at registration; rejected batches do not allocate IDs.
  Python `Connection` reads actual database metadata. Standalone planners, native
  callers, and the CLI must provide a catalog; the CLI accepts `--catalog PATH`.
- Native policy registration and catalog-free constructors are now fallible.
  See `developer-docs/policy-registration-catalog.md` for API details.
- A policy applies only if every ordinary SOURCE contributes. Source-attribute
  shorthand requires uniqueness for the entire policy, including OR/NOT cases.
- No-SINK `_OUTPUT_` policies apply only when every referenced public output
  attribute exists. Missing attributes skip the whole policy; present NULLs do not.
- Replace the earlier candidate/preflight retry wrapper with native feedback
  handling. Tool dispatch occurs only after a successful protected query.

### Limits

SQLite RETRY requires SQLite 3.44 or newer and buffers results before returning a
cursor. DataFusion RETRY is SELECT-only. PostgreSQL preserves existing helper
setup commits and rollback-on-error behavior. ClickHouse and Umbra do not support
RETRY. Unsupported provenance/binding shapes fail explicitly; this release does
not promise arbitrary SQL coverage. See `developer-docs/tool-retry.md`.
