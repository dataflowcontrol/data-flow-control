# Native RETRY and single-attempt tool dispatch

`ON FAIL RETRY('message')` is a native PGN resolution, not an opt-in conversion of
KILL. A rejected statement raises `RetryViolation`; its `failures` contains only
actually failing policies, ordered by their stable registration IDs. Messages
are preserved exactly, including whitespace, Unicode, and SQL-escaped quotes:

```python
from data_flow_control import Policy, Resolution, RetryViolation

policy = Policy.from_pgn(
    "SINK SendInput CONSTRAINT SendInput.recipient = 'approved@example.com' "
    "ON FAIL RETRY('Don''t send to another recipient')"
)
# Equivalent constructor fields:
# on_fail=Resolution.RETRY, retry_message="Don't send to another recipient"
```

This follows the policy-specific feedback intervention in
[Environment Steering §3.3.2](https://arxiv.org/html/2609.35807v1#S3.SS3.SSS2).
The host may use the feedback to construct a **new** SQL proposal. The library
never automatically reruns the rejected query or calls a model.

## Implementation scope

Threshold-dominance pruning applies only to REMOVE predicates. Distinct RETRY
policies retain their identities even when their constraints overlap.

Native RETRY transport is implemented for DuckDB, SQLite, PostgreSQL and
DataFusion (SELECT-only for DataFusion). ClickHouse and Umbra reject RETRY at
registration rather than silently converting it into filtering or generic KILL.
See the README supported-feature matrix for verification and transaction limits.
Unsupported query/lineage shapes fail explicitly; arbitrary SQL/backend coverage
is not an established guarantee.

## Exact failures and identity

```python
try:
    conn.execute("INSERT INTO SendInput (recipient) SELECT 'wrong@example.com'")
except RetryViolation as error:
    for failure in error.failures:
        print(failure.policy_index, failure.message)
        # failure.policy is the exact registered policy resolved by stable ID.
    messages = error.messages  # exact strings, sorted by stable policy ID
```

Failure identity is recorded by an execution-local native guard callback (or
PostgreSQL's dedicated SQLSTATE plus structured diagnostic payload) and resolved
through the Rust policy store. Python does not infer RETRY by matching
arbitrary backend exception text. Repeated failures of the same policy are
deduplicated by ID; different policies with identical messages remain separate.
For one failure, `message`, `policy_index`, and `policy` are convenient singular
attributes, and `str(error)` is exactly that message. These attributes are `None`
for multiple failures; use `failures`/`messages` rather than treating a generic
exception summary as policy feedback. Backend/parser/planner errors are not
policy violations.

## One proposal per tool-runner call

```python
from data_flow_control.tools import RetryToolRunner, ToolSpec, ToolUseDatabase

sent = []
def send(recipient):
    sent.append(recipient)  # local simulator, not a real email service
    return {"sent": recipient}

db = ToolUseDatabase([
    ToolSpec("send", "Send", {"recipient": "VARCHAR"},
             {"sent": "VARCHAR"}, send, None, None),
])
db.register_policy(policy)
runner = RetryToolRunner(db)
try:
    rejected = runner.run("send", {"recipient": "wrong@example.com"})
    assert rejected["status"] == "retry"
    assert rejected["messages"] == ["Don't send to another recipient"]
    assert sent == []
    # An explicit, new host-submitted proposal; no automatic loop or replay.
    accepted = runner.run_sql(
        "send", "INSERT INTO SendInput (recipient) SELECT 'approved@example.com'"
    )
finally:
    db.close()
```

Each invocation submits one checked insertion. There is no candidate-query
preflight, callback retry loop, `RetryDirective`, or `max_retries`. This promises
one query submission, **not** exactly-once evaluation of every SQL expression or
predicate. The host owns retry limits, scheduling, model transport, and context.

A tool is dispatched only after the checked insertion completes successfully.
A RETRY violation aborts the statement rather than retaining passing rows. A
successful insertion producing more than one row is rolled back by this
single-action tool API before any dispatch; zero rows mean `no_action`. REMOVE
remains filtering and does not fabricate RETRY feedback. Dispatch uses the exact
checked scalar values, including NULL. Ordinary omitted arguments are filled
from inspectable Python scalar defaults before checking; SQL proposals must
explicitly supply every declared argument column.

The SQL interface allows only one unqualified same-sink `INSERT ... SELECT`,
explicit argument columns, allowlisted local source tables, and a restricted
pure-function subset. No VALUES, metadata writes, other sinks, RETURNING,
multiple statements, subqueries/CTEs/set operations, or custom functions in
agent-generated SQL are accepted by this runner. Native connection support is a
separate, broader interface and must not be confused with this runner's subset.

## Visibility and transactions

Use a dedicated database with only one caller's visible state. `readable_tables`
grants access to **all rows** in those named tables; do not include hidden or
other callers' data. Feedback includes authorized schemas and explicitly supplied
JSON-serializable `context`, not automatically fetched records. Context must fit
`max_context_chars` (default 16000). Policy text/messages are trusted disclosures.

Policies must be registered before constructing the runner and remain fixed.
Its default sink-policy-presence check is not proof of policy adequacy. Exact
tool routing must be resolved before invocation. Do not concurrently access its
database through raw connections or other wrappers; its lock is not a tenant
security boundary.

The runner owns its transaction. Calling it inside an existing transaction is
unsupported: DuckDB's nested-BEGIN error can abort earlier uncommitted writes.
The runner does not implicitly commit or roll back a transaction it failed to
open, and it provides no savepoint isolation. Direct native execution likewise
inherits the backend's transaction-abort behavior.

Tool/audit failures after dispatch are terminal and may have uncertain external
outcomes. Database rollback cannot undo an email, payment, or remote API effect.
An optional trusted `action_transaction` hook is only for local simulators; it
must propagate failures and have a non-failing successful exit. It does not
provide distributed transactions. Never automatically replay such failures.

## Accepted placement rules and remaining decisions

- An ordinary multi-SOURCE policy applies only when the result has at least one
  contributor from **each** listed source. Required-source behavior is scoped to
  its established declared-sink semantics; source-only REQUIRED behavior is not
  silently changed.
- Scalar-source uniqueness is a precondition on the **entire** policy, including
  policies containing OR or NOT. Aggregate provenance must preserve actual
  contributors, multiplicity, and the distinction between present NULL and an
  absent source. Existing SQL NULL behavior must not be silently redefined.
- Resolution precedence is KILL, then REMOVE, then RETRY, then custom UDFs.
  REMOVE filters rejected rows before RETRY evaluates surviving rows; KILL must
  not be hidden by that filtering.
- KILL/REMOVE are splittable; RETRY is not. Registered UDFs accept an explicit
  strict-boolean `splittable` flag (default `False`). Custom-resolution ordering
  follows successful **function definition** registration, not policy order.
- All non-splittable policies form one joint logical resolution stage, including
  different resolution kinds. Splittability permits an optimization; it does not
  permit changing applicability or evaluating effects on eliminated rows.
  WHERE is an explicit resolution-pushdown barrier. JOIN and LIMIT candidate
  elimination must likewise preserve end-query semantics.
- Custom-policy failures are determined once on the original post-REMOVE stage.
  Selected repairs compose in successful function-definition registration order;
  predicates are not reevaluated after earlier repairs. Policies sharing one
  function are combined into one application. The legacy relation callback
  protocol still supplies `False` for applicable passing input and skips all-NULL
  or empty input; callback invocation with `False` is not selection for repair.

These rules require end-to-end placement tests. They do not establish general
policy coverage, faithfulness of generated policies, or provenance through
opaque LLM transformations.


## Current query coverage and explicit limits

The following describes the implemented draft, not a claim of arbitrary SQL
coverage. The tool runner's deliberately smaller SQL subset above is unchanged.

| Path | Current bound |
| --- | --- |
| SELECT / INSERT SELECT | Catalog-bound transparent projection/filter/CTE/derived inputs and root policy aggregation over transparent input. |
| JOIN | Inner/cross joins; transparent LEFT/RIGHT/FULL ON joins carry a source-presence marker created before padding. Actual NULL contributors remain distinct from absent sources. Aggregation over nullable joins is not covered by this extension. |
| UNION ALL | Aligned branch provenance, joint final effects, and tested branch/root ordering and limits. |
| Terminal UNION DISTINCT / nonaggregating SELECT DISTINCT | Group by public fields only, preserving contributor values and source presence before final policy evaluation; hidden policy fields do not change duplicate identity. |
| Terminal INTERSECT DISTINCT | Disjoint source origins and deterministic MIN/MAX or source-uniqueness constraints only. Public types are aligned before NULL-safe key pairing; additive aggregates, ALL, shared origins, nested finalized state, and unproven scalar-function purity are rejected. |
| Terminal EXCEPT DISTINCT | Positive-side raw contributors survive NULL-safe public-key exclusion, then final policy evaluation. Applicable negative-side SOURCE dependencies reject with policy identity. ALL, nested differences and shared positive/negative source occurrences remain unsupported. |
| Custom resolution effects | Fixed original failure flags; sequential tuple/relation effects in successful function-definition registration order. Materialized effect stages prevent duplicated callbacks; no candidate query replay. |

Unproven joint-provenance paths fail explicitly rather than resolving separate
branches and returning incomplete RETRY feedback. These include opaque nested
aggregation/DISTINCT, aggregation over set operations, recursive/nested WITH,
expression subqueries/windows, NATURAL/USING annotation transport, repeated
protected source occurrences, unproven public expression labels (explicit
aliases can be used), and DIMENSION-dependent terminal DISTINCT. Existing
splittable-only legacy paths remain where separately classified; their presence
is not a proof of universal end-query equivalence. Catalog-free merged-source
set operations require provenance support rather than source-local predicate
splitting. Applicable UPDATE/DELETE/MERGE or VALUES RETRY writes are not covered.

## Logical stages versus compiler traversals

The accepted rules above were clarified on 2026-10-26. The implementation aims
for inherited source/evidence demand on descent and synthesized bindings and
annotations on ascent. The new joint visitor counts original **Query** nodes,
memoizes CTE-definition results, and does not reinstrument generated scan
wrappers or resolution CTEs. These counters do not claim that every AST
expression or every legacy optimizer path is visited only twice: catalog/schema
binding, identifier hygiene, scope-local qualified-name normalization, and older
nullable-branch/derived/limit rewrites have separate work.

There are real logical boundaries without a demonstrated need for extra
provenance-instrumentation passes: WHERE/candidate elimination before effects;
KILL, REMOVE survivors, RETRY feedback and ordered repairs; and duplicate-key
collapse before aggregate policy evaluation. A predicate needing attributes
from multiple sources can only be finalized where those attributes coexist.
These boundaries may produce multiple SQL CTEs, but SQL execution stages are
not additional compiler traversals. General non-distributive aggregate regions
and legacy traversal debt are not silently claimed solved by this draft.

### Registration and EXCEPT analysis

Every declared physical SOURCE/SINK needs an explicit catalog entry, and
referenced columns are checked atomically at registration. Python connections
synchronize actual database metadata; standalone planners require a catalog
snapshot. See [catalog registration](policy-registration-catalog.md).

EXCEPT dependency discovery currently adds a pure source-scope analysis walk
before joint instrumentation. This is implementation organization debt, not a
proof that another provenance instrumentation pass is necessary; its scope/CTE
dependency state could be fused into inherited analysis. Generated anti-filter
SQL is not instrumented again. The new joint Query down/up counters exclude
this binding scan and do not certify all legacy optimizer traversals.
