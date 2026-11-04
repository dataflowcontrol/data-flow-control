# Policy intermediate propagation

`data_flow_control.intermediates` provides a trusted Python/DuckDB boundary for
carrying aggregate state between relational operations and tool calls. It is an
explicit adapter API: it does not yet compile PGN constraints or automatically
instrument `Connection.execute`. The existing Rust rewriter is unchanged.

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
inference algorithm; it does not expand existing support for EXCEPT, windows,
nullable joins or arbitrary SQL transformations.

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

Limitations: explicit trusted integration only; DuckDB scalar values and SQL
types; no persistence/serialization contract, automatic PGN decomposition,
DISTINCT additive aggregates, arbitrary aggregate/UDF state, cross-source joint
expressions, source mutation, or reconstruction/splitting of input states.
Source-local expressions must be deterministic and use ordinary SQL equality;
custom collations and volatile functions are outside this contract. Tool SQL
model registration, actual-output correspondence and automatic capture belong
to the separate tool-provenance work.
