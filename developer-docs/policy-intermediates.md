# Policy intermediate propagation

`data_flow_control.intermediates` provides a trusted Python/DuckDB boundary for
carrying aggregate state between relational operations and tool calls. It is an
explicit state API, plus the opt-in automatic `Connection.execute` path described
below. Rust owns policy compilation, state representation/capture/merge, contributor
resolution, token storage and policy resolution decisions. Python is a database
execution/API adapter; it does not parse or interpret intermediate policies.

`compile_policy_intermediates(raw_connection, policy)` in
`data_flow_control.intermediate_policy` derives a plan and final predicate from
an existing `Policy`. Its `.plan` supplies `capture` and `merge`;
`.evaluate(state, sink={...})` binds the real sink column values. Compilation
supports source-local expressions, implicit unique source comparisons,
MIN/MAX/SUM/COUNT/AVG/BOOL_AND/BOOL_OR, and the parser's UNIQUE/NOT UNIQUE
expansions. It rejects DIMENSION, subqueries, windows, filtered aggregates,
unknown aggregates, cross-source expression demands and negated implicit
uniqueness. These explicit errors must not fall back to untracked execution.

A `PolicyIntermediates` plan declares multiple source-local `Intermediate`s.
`capture(source, query, identity_columns=...)` evaluates each full contributor
expression in SQL before aggregation. This preserves correlation, for example
`BOOL_AND(category <> 'Discrimination' OR priority = 'High')`, without retaining
individual attribute values or zipping independent sets. Each capture describes
the contributors of one output row. A trusted relational adapter supplies that
correspondence, using the existing rewriter's supported contributor semantics.

Projection, filtering and 1:many output copy the entire immutable state. Joins,
groups and duplicate-eliminating operations call `merge` on their contributing
input states. Dropped rows contribute nothing. A nullable join supplies no state
for an absent side. This API supplies the merge operation, not an SQL lineage
inference algorithm; it does not by itself infer EXCEPT, nullable joins or arbitrary transformations.
The automatic path below derives supported aggregate-window contributors in SQL.

```python
from data_flow_control.intermediates import Intermediate, PolicyIntermediates

plan = PolicyIntermediates(raw_duckdb_connection, [
    Intermediate('case_id', 'cases', 'cases.id'),
    Intermediate('routing', 'cases',
                 "category <> 'Discrimination' OR priority = 'High'", 'bool_and'),
], required_sources=['cases'])

send_input = plan.capture('cases', "SELECT * FROM cases WHERE id = 'B'",
                          identity_columns=['id'])
send_output = send_input  # trusted 1:1 tool declaration; do not split the state
update_input = plan.merge(send_output)
allowed = plan.evaluate(update_input,
    'case_id__cardinality = 1 AND case_id = update_case AND routing AND sent',
    sink={'update_case': 'B', 'sent': True})
```

`evaluate` returns `True`/`False`, or `None` for an inapplicable ordinary SOURCE
policy. Missing REQUIRED sources fail. The harness must apply the policy's
existing resolution before executing effects; a successful verdict is not proof
that a send occurred. Actual output success remains a separate sink/history fact.

State uses bounded fields per expression: UNIQUE has empty/one/conflict among
non-NULL values; MIN/MAX/BOOL_AND/BOOL_OR have one field; SUM and COUNT have one;
AVG has sum and non-NULL count. SQL NULL behavior is retained. UNIQUE ignores
NULL like the existing COUNT(DISTINCT) semantics. Final predicates explicitly
include the uniqueness cardinality check required by their policy.

Contributor identities are additional, potentially unbounded overlap metadata;
this is not a claim of constant total memory. Identities must identify immutable
contributors in one source snapshot, not output positions or attribute values.
Repeated contributor keys introduced by joins are deduplicated before capture;
distinct contributors with equal values are retained. Repeated and contained
whole states are deduplicated during merge. Idempotent states can merge partial
overlap. SUM/COUNT/AVG reject partial overlap: aggregate totals cannot recover
an intersection's contribution. There is no hidden reconstruction of individual
contributors. All captures in a plan must use the same immutable snapshot.

Low-level limitations: trusted integration; DuckDB scalar values and SQL
types with checked lossless Python transfer; no persistence/serialization contract, full PGN coverage,
DISTINCT additive aggregates, arbitrary aggregate/UDF state, cross-source joint
expressions, source mutation, or reconstruction/splitting of input states.
Source-local expressions must be deterministic and use ordinary SQL equality;
custom collations and volatile functions are outside this contract. Tool SQL
model registration, actual-output correspondence and automatic capture belong
to the separate tool-provenance work.

## Automatic execution (opt-in)

The Python DuckDB connection now supports automatic capture and propagation for
a bounded relational subset. Load initial rows and register policies, then call
`connection.enable_intermediate_propagation()`. Ordinary `execute`, `fetchall`
and `fetchone` then propagate states through direct SELECT, projection/filter,
inner/cross joins, GROUP BY/global aggregates and HAVING, nonrecursive CTEs,
aliased derived tables, and aggregate windows. CREATE TABLE AS SELECT (including
TEMP) and INSERT SELECT persist these states. REMOVE filters complete fixed
output rows; KILL aborts before writing. RETRY raises the existing `RetryViolation`
with the actual failing policies and their stable registration IDs, before writes.
KILL takes precedence; REMOVE-discarded outputs do not trigger RETRY.
For example, after registering a policy connecting `cases.id` to `updates.case_id`:

```python
connection.enable_intermediate_propagation()
connection.execute("CREATE TEMP TABLE input AS SELECT id FROM cases WHERE id = 'B'")
connection.execute("CREATE TABLE sent AS SELECT id, TRUE AS success FROM input")
connection.execute("INSERT INTO updates SELECT id, success FROM sent")
```

There is no manual state construction in this path. Each statement runs in a
transaction: data reads, state capture, sink checks and writes share a snapshot;
failures roll back data and token allocation. INSERT sink checks see values
coerced to the actual target types. State tokens are stored in a reserved physical
column, excluded from logical stars and the connection's catalog. Explicit
metadata access, dynamic metadata projections and whole-row table expressions
are rejected. Duplicate-valued output rows retain distinct tokens, so matching
states does not depend on output ordering or equality of public values.

`PassantRewriter.rewrite_intermediate_query` uses the native SQL AST, ordinary
`TableCatalog` and `AggregateRegistry`. The catalog's `intermediate_bindings`
records trusted source-rowid and persisted-token channels independently of
logical columns. CTE and derived scopes carry the generated channel, so a CTE
named like a source cannot manufacture source authority.

`intermediate_compile` lowers the existing registration-time `CompiledPolicy` AST
into state demands and a final predicate, using core source/sink aliases and
`AggregateRegistry`. Original single-parse AST metadata is retained so unsupported
AVG modifiers cannot disappear during existing AVG decomposition. The standalone
trusted-plan API uses the same compiler without claiming catalog registration.
There is no SQLGlot policy compiler or separate scalar-function whitelist.

`intermediate_state` owns opaque states, SQL-typed scalar transport, capture/merge
SQL, overlap handling, required-source applicability and final predicate evaluation.
`intermediate_session` resolves contributor dependencies and owns opaque stored
state tokens, ownership checks and transaction rollback checkpoints. The core
`SqlExecutor` trait supplies database execution without a Python dependency.
Values use SQL type plus lossless text/NULL encoding; rebinding uses encoded text
rather than Python scalar equality. Identity type changes fail closed.

`intermediate_execution` plans SELECT/INSERT/CREATE and chooses KILL/REMOVE/RETRY
using the existing `ResolutionRegistry` precedence and stable policy identities.
Python executes these plans, performs catalog/database-identity checks, manages
the DB-API transaction/cursor and translates native effect results to existing
exceptions. PyO3 transports values and invokes the supplied DB connection; it
contains no aggregate merge or policy interpretation rules.

The state-aware SQL lowering is still a dedicated native path. It shares the core
policy compiler, AST, catalog, aggregate and resolution registries; it does not
claim full operator parity with the legacy joint-lineage traversal.

Aggregate windows union the current row's dependencies with those of each
referenced frame, deduplicating contributor identities. Multiple inline frames
are supported. ROW_NUMBER is rejected: a public row's ordinal can depend on
a secret predecessor, so row-local state is insufficient. Other navigation
and ranking functions, filtered windows, named windows, and grouped plus window
expressions in the same SELECT fail closed. Group first in a CTE and window over
its output instead. Positional parameters are numbered before cloning frames.

Unsupported SQL fails instead of falling back to untracked execution: expression
subqueries, recursive CTEs, set operations, DISTINCT/QUALIFY, nullable/NATURAL/USING
joins, UPDATE/DELETE, explicit transactions, INSERT VALUES, partial-column INSERTs,
and custom resolutions remain excluded. Nested computed outputs need explicit
aliases (or CTE column names). Schema-qualified tables are not yet supported.
Policy changes require a fresh session; source mutation and raw writes outside
this connection are outside the trusted contract.

This path preserves already-computed payloads and checks their whole contributor
states. It does not implement legacy REMOVE's ability to split contributors and
recompute an aggregate or window result. That would violate the opaque-input
contract after a tool boundary. Native resolution treats this path as fixed
payloads. Broader integration with legacy recomputable stages must preserve that
distinction; feeding only scalar aggregates into scan filters would be unsound.
The dedicated native lowering currently fails closed on unsupported operators
and partial-overlap additive merges.

Queries must be deterministic and use ordinary SQL equality/collation. Fetch
results are materialized and support `description`, `fetchone`, `fetchmany` and
`fetchall`; this is not a streaming or full DuckDB cursor API. The serialized
SQL from `transform_query` cannot carry process-local states and is disabled in
this mode. Persisted tables must not be reopened in another session: the
constructor rejects the reserved metadata column rather than accepting orphan
states. There is no disk persistence or cross-process state codec yet.

The public table values are materialized through Python in this mode. To avoid
lossy conversions, both public values and captured identities/expressions are
restricted to Boolean, integer, floating-point (values only), VARCHAR, BLOB and
DECIMAL SQL types. Temporal, ENUM and container types are rejected before fetch;
notably TIMESTAMP_NS is **not** silently truncated to Python microseconds.
Floating-point identities are rejected. An explicitly chosen lossless SQL scalar
encoding can be used where appropriate; e.g. TIMESTAMP_NS::VARCHAR retains
nanoseconds. No automatic type-changing encoding is imposed on a policy.

## Bridge for the separate SQL tool-provenance adapter

The tool declaration remains in the other PR. The concrete Rust interface is:

- `PassantRewriter::compile_intermediate_policy(stable_id)` returns native demands
  from the registered policy; `compile_intermediate_definition` supports trusted
  standalone definitions without registering enforcement authority.
- `IntermediatePlan` captures, merges and evaluates `IntermediateState` through
  `SqlExecutor`. States are opaque Rust-owned objects, not Python dictionaries.
- `IntermediateSession::row_states`, `merge_rows`, `register_rows`, `checkpoint`
  and `rollback` resolve whole-input dependencies and manage session tokens.
- `PassantRewriter::plan_intermediate_statement`, `intermediate_policy_indices`
  and `resolve_intermediate_rows` provide native SQL/effect plans.

The Python tool-facing API remains compatible; it passes opaque native states:


1. Execute the tool-input query normally. `result.intermediates` aligns with the
   result's original row order (independent of subsequent fetch position).
2. For each modeled output row, use `connection.merge_intermediate_inputs(*rows)`
   on the policy-state tuples of its contributing inputs. An empty input set
   produces empty states. A 1:1 result can reuse the tuple directly.
3. After the actual tool succeeds and its output correspondence is verified,
   call `connection.store_intermediate_outputs(table, output_rows, states)` to
   record observations in a declared output table. State ownership and row
   alignment are validated; storage is transactional. This trusted bridge does
   not invoke tools or authorize their effects: input policy checks must occur
   before the tool call. A later ordinary SELECT/INSERT sees the inherited state.

External observations remain new sources only where declared as such. Tool
mapping must account for real external dependencies; the bridge cannot infer
unmentioned reads. The bridge functions are harness APIs, never model tools. Unlike ordinary SQL,
`store_intermediate_outputs` is an explicit authority to append externally
observed facts, including to a declared source relation. The trusted harness must
validate that the target is the actual tool's output relation and that the values
are actual observations; exposing this bridge as a model-writable SQL endpoint
would violate the authority boundary.

Declared sources are pinned to DuckDB database/schema/table catalog IDs at
initialization. Each automatic statement validates those bindings inside its
transaction, including checking for same-name table or view shadows. Ambiguous
initial bindings, replaced tables and added shadows fail closed. The trusted
observation bridge validates the same pins; if it installs its own metadata
column, it refreshes only that known target's changed catalog ID and rolls the
pin back on failure. This detects physical rebinding, not arbitrary raw DML:
external raw updates to an existing table remain outside the trusted contract.
