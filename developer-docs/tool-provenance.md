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
       FROM ToolInput i
       CROSS JOIN (VALUES (1), (2)) n(copy_number)
       WHERE n.copy_number <= i.number_of_copies""",
    key_columns=("id", "copy_number"),
)
model.insert_outputs(connection, "ToolOutput", [
    ToolOutput((7, 2), {"message": "copy"}),
    ToolOutput((7, 1), {"message": "copy"}),
])
```

This example expands each input to up to two copies. A larger expansion can
join an appropriate relation of occurrences.

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

Joins, self-joins, grouping, CTEs and lateral expansion can describe modeled
rows. `CROSS JOIN LATERAL generate_series(...)` supports policy-bearing expansion;
comma-lateral spelling is normalized before the Rust parser. Source-to-output
comparisons are tested for 1:1 and 1:many. Safe model subqueries are flattened
with SQLGlot so actual outputs remain in policy scope.

Aggregate windows support KILL and whole-output REMOVE policies over one common inline contributor
frame that includes the current row. Policy aggregates receive that same OVER
clause, preserving the original window values. For example, a cumulative SUM
with a source-category constraint checks every contributor to each cumulative
frame, rather than only its current row. Mixed/different frames, named windows,
grouped/HAVING/QUALIFY window inputs and other aggregate-window resolutions
are explicitly unsupported. Value/navigation windows such as LEAD, LAG and
FIRST_VALUE are rejected when a policy applies; they are not silently treated
as row-local provenance. The core retains its established ROW_NUMBER behavior.

### Fixed payloads cannot be recomputed after filtering

An ordinary SQL query can filter a contributor and recompute its aggregate,
window or scalar-subquery value. An external tool's already computed payload
cannot be recomputed that way. Keeping its output key is not sufficient evidence
that its original contributors passed policy checks.

Publication therefore fails closed, before normalization, when registered
policies coexist with scalar/expression subqueries (including ARRAY queries).
REMOVE uses ordinary filtering for row-preserving models, including self-joins.
For a single flat aggregate or supported aggregate-window SELECT, it instead
checks the original complete group/frame and discards the entire modeled output
if that check fails. A scalar REMOVE constraint is evaluated for each original
contributor using `BOOL_AND((constraint) IS TRUE)`; NULL contributors cannot be
silently ignored. Explicit aggregate constraints keep their authored meaning.
Multiple contributor predicates are evaluated together before any output is discarded.
A read-only preflight checks sink-only KILL predicates on the original actual
outputs before contributor filtering can hide an abort. It uses the ordinary
INSERT rewrite's source query without writing rows. The core's resolution
precedence remains KILL, then REMOVE, then RETRY.
For example, a group containing public and private rows produces no external
output under `category = 'public'`, rather than publishing the original payload
after removing the private contributor. A sliding window loses just outputs
whose original frames fail, without recomputing surviving frames.

This uses DFC's existing tuple-resolution stage with an internal terminal-remove
marker in a temporary planner; registered user policies remain unchanged.
Nested relational stages, DISTINCT, set operations and outer joins still fail
closed with non-row-preserving REMOVE: resolving an inner stage could change a
later output's contributor set. Value/navigation windows still need their own
contributor derivation. Source-only aggregate/frame checks and sink-only checks
are supported; a constraint directly comparing original aggregate contributors
with an actual output value still lacks deferred predicate binding across the
model/output boundary and fails closed. This limitation is distinct from the
working input/output comparisons on 1:1 and 1:many row models. These are current
core integration boundaries, not a claim that such SQL cannot express provenance. Checks currently consider all
registered policies conservatively, even if a particular policy might be
inapplicable to this call. Repair/UI/UDF resolutions are also rejected;
the publisher cannot recompute or repair an external payload. KILL and RETRY
retain their abort behavior within the ordinary rewriter's supported SQL scope.

Regression tests cover the otherwise unsafe public-row/private-LEAD payload,
same-source and distinct-source scalar subqueries, and unchanged secret-bearing
string aggregates after REMOVE. Aggregate regression cases now discard the
whole output; unsupported expression-subquery and navigation cases still reject
and roll back publication. This boundary is narrower than accepting arbitrary
SQL that happens to execute.

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
before publication. Use positional `?` parameters for call-scoping values. They are numbered before
normalization so moving predicates cannot change parameter binding. Do not call within
an existing transaction; DuckDB's nested-BEGIN error can abort that transaction.

Call this API only for successful, complete external results. Failed calls or
partial results require a separate declared outcome model if they are to become
policy-bearing data. The helper does not publish a success row for a failure,
retry a tool, or roll back an already-performed external effect. A validation,
rewrite or insertion failure rolls back database publication. Apply policies
that authorize side effects before executing those effects; publication-time
checks are not a replacement for those guards.

## Integration with intermediate propagation

The generated INSERT preserves the SQL model's relational semantics, rather
than materializing it into an unlabelled table or rebuilding original
contributors. Simple subqueries may be flattened before `Connection.execute`.

The companion [propagation PR #8](https://github.com/dataflowcontrol/data-flow-control/pull/8),
reviewed at `1bfb3cc70cc5b6e91102e535bc87d74b5e646184`, now provides opt-in
propagation through normal `Connection.execute`, plus trusted observation storage.
For its supported relational subset, the model query itself derives the whole
input states for each result row; the harness does not declare input-ID mappings.

Use a fresh model result with `bind_intermediates` to align those states with
actual output occurrences:

```python
result = connection.execute(model.sql)  # automatic propagation is enabled
states = model.bind_intermediates(result, actual_outputs)
# Output tuples must follow the declared ToolOutput table's public column order.
connection.store_intermediate_outputs(
    "ToolOutput", [(output.values["message"],) for output in actual_outputs], states,
)
# Subsequent ordinary SQL carries inherited states and enforces its sink policies.
connection.execute("INSERT INTO FinalSink SELECT message FROM ToolOutput")
```

The binding helper rejects missing/duplicate keys and partially consumed results.
It reorders whole state tuples and does not inspect original source contributors.
A singleton model uses an empty key; expansion may reuse a whole input state;
join/group states are already merged by the automatic relational adapter.

`store_intermediate_outputs` records trusted observations; it does not authorize
external effects or evaluate output-sink policies against their actual payloads.
Use the appropriate pre-effect guards and ordinary downstream sink operations.
It is **not** a silent substitute for `insert_outputs`, which enforces policies
using the legacy SQL rewriter. Calling `insert_outputs` with automatic propagation
enabled raises an explicit error before starting a transaction. This avoids the
two adapters' competing transaction ownership and their different SQL subsets.

The optional `python/tests/test_tool_provenance_intermediates.py` is run in an
isolated worktree containing PR #7 and PR #8 files without a Git merge. It covers
normal CTAS/grouping of initial sources into a tool input, SELECT-star and
physical-relation expansion models, reordered duplicate actual payloads,
whole-state propagation through the observation bridge, and downstream ordinary
INSERT accepting/rejecting correlated source-expression and actual-sink checks.
The module skips on standalone PR #7; its tests and the lower-level explicit
state tests establish actual compatibility against the pinned companion revision.

Automatic mode currently rejects CTEs/subqueries/windows/set operations,
DISTINCT/HAVING, nullable joins and lateral/VALUES table expansions. Those remain
separate from the legacy publisher's supported scope. Use a physical expansion
relation in the tested automatic 1:many path. Source states are session-local;
there is no cross-process persistence contract. The two PRs remain separately
reviewable, and a harness must explicitly select the appropriate execution path.

The existing LangChain tool database remains opt-in/unmodified: its raw output
logging does not automatically invoke this interface. A trusted harness calls
`insert_outputs` explicitly; models and keys must never come from an untrusted
agent. No package version change or model API calls are needed.
