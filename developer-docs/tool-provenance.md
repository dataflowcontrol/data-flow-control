# SQL-defined tool provenance

`ToolProvenance` is a trusted harness interface for publishing externally computed
rows with the provenance of a SQL query. It does not execute the external tool,
implement its business logic, or authorize its side effects. The query remains
an ordinary relational input to DFC's `INSERT … SELECT` rewrite; this is not an
input/output pair language.

## One input, one output

```python
from data_flow_control import ToolOutput, ToolProvenance

model = ToolProvenance("SELECT * FROM ToolInput WHERE id = ?")
# The harness executes the external tool and observes success separately.
model.insert_outputs(
    connection, "ToolOutput",
    [ToolOutput(key=(), values={"message": "sent"})],
    params=[input_id],
)
```

`SELECT * FROM ToolInput` suffices if the input relation contains one row.
With no `key_columns`, exactly one modeled row corresponds to the one actual
output. Neither the projected model values nor the SQL model need to compute the
actual message. Every input row is indivisible; this API never reopens or selects
among that row's previously contributing source data.

## Expansion and explicit output occurrences

```python
model = ToolProvenance(
    """SELECT i.*, n.copy_number
       FROM ToolInput i,
       LATERAL generate_series(1, i.number_of_copies) n(copy_number)""",
    key_columns=("id", "copy_number"),
)
model.insert_outputs(connection, "ToolOutput", [
    ToolOutput((7, 2), {"message": "copy"}),
    ToolOutput((7, 1), {"message": "copy"}),
])
```

The harness supplies an out-of-band occurrence key with each actual row. Keys
are model result columns, not business output columns: no additional business
schema fields or benchmark-specific tables are required. Reordered results and
duplicate payloads are fine; duplicate keys are rejected. Missing/extra outputs,
NULL/NaN keys, a key arity mismatch, and SQL equality ambiguity are rejected before
publication. Empty modeled and actual relations are valid. Correspondence is
checked before DFC resolutions such as REMOVE; those may remove matching rows.
The return value is the number actually inserted.

Use stable occurrence identifiers if two modeled rows would otherwise have the
same key. Do not zip unordered query results with a tool's returned list. A
window-derived occurrence key requires a deterministic total order, including
ties. A nondeterministic tool may return different payloads each time, but its
observed output occurrences must still match the declared model. If observed
choices determine cardinality or lineage, provide those observations as trusted
input data and include them in the model.

## Relational scope and external data

Joins, self-joins, grouping, CTEs, windows and lateral expansion can describe
modeled rows. Their policy behavior is exactly that of the existing SQL rewrite
engine; this interface does not expand its supported policy/SQL combinations.
SQL acceptance alone is not evidence that every window dependency is captured.
Unsupported combinations may raise the existing rewrite error.

Every external data source read by the tool must be represented in its model,
using a trusted relation/snapshot with the appropriate DFC source policies and
provenance. For example, a search reading case records must join its request to
the relevant case snapshot; `SELECT * FROM ToolInput` only models the request's
contribution. The harness must establish correspondence with the data actually
read. DFC cannot infer undisclosed network reads or verify that a declaration is
faithful to external execution.

This initial implementation uses DuckDB and owns a transaction on an exclusively
used connection. The SELECT is evaluated for correspondence validation and again
for policy-aware insertion against the same transaction snapshot. It **must be
deterministic** across those evaluations: volatile functions, stateful UDFs,
mutable file/network scans and nondeterministic LIMIT/tie ordering are outside
the contract. Known `random()`/`uuid()` uses are rejected early, but that check is
not a determinism proof or an SQL sandbox. Materialize external observations
before publication. Use parameters for call-scoping values. Do not call within
an existing transaction; DuckDB's nested-BEGIN error can abort that transaction.

Call this API only for successful, complete external results. Failed calls or
partial results require a separate declared outcome model if they are to become
policy-bearing data. The helper does not publish a success row for a failure,
retry a tool, or roll back an already-performed external effect. A validation,
rewrite or insertion failure rolls back database publication. Apply policies
that authorize side effects before executing those effects; publication-time
checks are not a replacement for those guards.

## Integration with intermediate propagation

The generated INSERT keeps the original SQL model in its FROM tree, rather than
materializing it into an unlabelled table or rebuilding original contributors.
The independent propagation implementation should consume that query via the
ordinary `Connection.execute` path and transfer each input row's complete
policy-intermediate state using its existing relational aggregation rules.
This PR alone checks policies from the model's directly named sources to the
publication sink. It does **not** add durable intermediate-state storage or claim
that those policies survive a subsequent query over the published output table.

The existing LangChain tool database remains opt-in/unmodified: its raw output
logging does not automatically invoke this interface. A trusted harness calls
`insert_outputs` explicitly; models and keys must never come from an untrusted
agent. No package version change or model API calls are needed.
