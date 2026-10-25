# Constraint-aware tool retry (opt-in)

`data_flow_control.tools.RetryToolRunner` implements a small, synchronous host-side
version of [Environment Steering §3.3.2](https://arxiv.org/html/2609.35807v1#S3.SS3.SSS2).
An ordinary tool call becomes a protected input-table insertion. Only after a
policy rejection does a caller-provided callback propose an `INSERT ... SELECT`
replacement into **the same tool-input relation**. Native DFC checks the replacement
and its actual SQL provenance before the tool runs.

This is not a Rust `RETRY` resolution: policies still use `Resolution.KILL`.
`Policy.from_pgn("... ON FAIL RETRY(...) ...")` is not supported. Existing
LangChain `observe` and `enforce` modes and `CallToolWithDataFlow` are unchanged.
In particular, `observe` is logging, **not** pre-dispatch enforcement.

## Model-neutral API

No LangChain install, agent framework, or model client is required:

```python
from data_flow_control import Policy, Resolution
from data_flow_control.tools import (
    RetryDirective, RetryToolRunner, ToolSpec, ToolUseDatabase,
)

sent = []

def send(recipient):
    sent.append(recipient)  # a local simulator, not a real email transport
    return {"sent": recipient}

db = ToolUseDatabase([
    ToolSpec(
        name="send", description="Send a message",
        input_schema={"recipient": "VARCHAR"},
        output_schema={"sent": "VARCHAR"},
        call_sync=send, call_async=None, original_tool=None,
    ),
])
db.register_policy(Policy(
    sources=[], sink="SendInput",
    constraint="SendInput.recipient = 'approved@example.com'",
    on_fail=Resolution.KILL,
))

# Bind to registered policies, because registration normalizes expressions.
registered = db.conn.policies()[0]
runner = RetryToolRunner(
    db,
    directives=[RetryDirective.for_policy(registered, "Use the approved recipient")],
    max_retries=2,
)

# Replace this scripted callback with your own model/agent transport if desired.
# The library itself makes no model calls.
def turn(request):
    assert request["required_sink"] == "SendInput"
    assert request["feedback"]["category"] == "policy_denial"
    return {"sql": "INSERT INTO SendInput (recipient) SELECT 'approved@example.com'"}

try:
    result = runner.run("send", {"recipient": "wrong@example.com"}, turn)
    assert result["status"] == "recovered"
    assert sent == ["approved@example.com"]
finally:
    db.close()
```

`run(tool, arguments, turn, *, context=None)` executes one logical action, with at
most `max_retries` corrective callback turns after the initial attempt. Its return
value is a structured terminal result and audit containing `attempts`, `turns`,
the resolved tool/sink, and a frozen policy-set fingerprint. Successful results
include the tool's output, so hosts should apply their normal serialization and
visibility rules. `runner.sessions` retains these records in memory; hosts own
persistence, retention and redaction. Validated attempts also use the existing
`__passant_tool_sql_calls` database log. Invalid SQL/protocol replies are recorded
in the session audit, not executed or inserted into tool tables.

Policies must be registered **before** constructing the runner and remain fixed.
`RetryDirective.for_policy(registered_policy, message)` is a convenience for
`RetryDirective(policy_fingerprint(registered_policy), message)`. Unknown or
non-KILL directives are rejected at construction.

### Explicit opt-in, not a global KILL conversion

An unmarked KILL remains `hard_stop`. Because the engine does not identify the
exact failed-policy subset, **all potentially applicable policies** must be KILL
policies with explicit retry directives. This deliberately conservative first
version also considers sinkless source policies potentially applicable. Feedback
labels them `applicable_policies`, not “the failed policies.” Mixed REMOVE/KILL
or other resolutions are not automatically made retryable.

By default, `require_sink_policy=True` returns `coverage_gap` before dispatch if
no policy declares the resolved sink. This checks policy presence, not policy
adequacy. A trusted host can explicitly set it to `False` for unprotected tools.
Aliases/dynamic routers must be resolved **before** calling this runner: it looks
up exact registered tool names and does not infer endpoint intent inside a generic
API tool's arguments. Put any required endpoint/recipient/route constraint into
the actual sink policy.

## Source-derived replacement and visibility

For a grounding policy, register its source table and require that source using
ordinary DFC `required_sources`. Then authorize model SQL to read the source table:

```python
runner = RetryToolRunner(
    db,
    directives=[RetryDirective.for_policy(p, "Derive recipient from observed contacts")
                for p in db.conn.policies()],
    readable_tables=["LookupContactOutput"],
)
# A callback may return:
# INSERT INTO SendInput (recipient)
# SELECT recipient FROM LookupContactOutput WHERE contact_id = 'c1'
```

This is a configuration pattern; the host must have actually created/populated
`LookupContactOutput` and registered a corresponding policy. The runner never
invents observation IDs or infers provenance through opaque LLM transformations.
Matching a literal to a previously observed value does not establish its SQL
provenance. Ordinary arguments are represented as literal `SELECT` expressions
(rather than the historical `VALUES` bypass path), so a required-source policy
can reject them and steer toward a source-derived query.

Use a database dedicated to one caller's visible state. `readable_tables` grants
SQL access to **every row** of those explicitly named local tables. Do not list
prepared/hidden data or tables mixing other callers' records. The default is no
readable source tables. Feedback contains only their schemas, the input schema,
policy definitions/messages, and a host-supplied `context` mapping. It never
automatically reads or discloses source/dimension records. `context` must contain
only information already visible to the caller, must be JSON-serializable, and
must fit `max_context_chars` (default 16000). Oversized context is rejected rather
than silently truncating evidence. Policy definitions/messages themselves are
trusted host-authored disclosures; do not embed hidden secrets in them.

## SQL and action boundaries

* A replacement must be one unqualified same-sink `INSERT ... SELECT` specifying
  every declared user argument column exactly once. No metadata writes, `VALUES`, other tools, DDL, mutations
  of existing rows, `RETURNING`, conflict/upsert modifiers, or multiple statements.
* This initial runner accepts a deliberately small deterministic SQL subset:
  local allowlisted tables, joins, predicates, grouping, and common pure scalar/
  aggregate functions. It excludes CTEs, subqueries, set operations, table functions,
  filesystem/network functions, time/random/sequence functions, and custom UDFs in
  agent SQL. Policy authors may still use trusted UDFs; their safety/determinism is
  the host's responsibility. There is no automatic function-allowlist expansion.
* A deterministic preflight rejects more than one candidate row before dispatch.
  Native DFC then executes the **original source-derived INSERT**, not a re-created
  literal insertion. Tool arguments are read from the checked inserted row,
  including SQL NULLs (rather than substituting Python callable defaults).
  For an ordinary call, omitted arguments are filled from inspectable Python
  scalar defaults **before** the check; missing required/uninspectable defaults
  are rejected. Explicit `None` remains SQL NULL and dispatches as `None`.
  SQL replacements must explicitly supply every declared argument, so omitted
  columns cannot silently become defaults. Non-scalar arguments/defaults and
  tools with no declared input columns are outside this initial API.
* Zero candidates are `no_action`. A nonempty candidate filtered to zero is not
  success: it is a policy denial when all applicable policies are KILL, otherwise
  terminal `filtered`. Neither dispatches a tool. This covers native KILL paths
  that return zero rows instead of raising a violation exception.
* Only pre-dispatch policy denials unlock corrective turns. Malformed/unsupported
  replacement SQL can receive separately labeled `invalid_sql` feedback within
  the **same** retry budget. Parser/binder/backend/planner failures during native
  execution are terminal `engine_error`, never business-policy violations.
* Exhaustion returns `exhausted`; no rejected action runs. Invalid callback
  envelopes return `protocol_error`, callback exceptions `callback_error`, policy
  mutation `policy_changed`, and missing coverage `coverage_gap`.

The callback is **trusted host code**, not a Python sandbox. It should only
construct/return SQL; it must not invoke tools, mutate policies/database/world
state, or recursively use this runner. Do not access the database concurrently
through other runners/raw connections. The runner rejects overlapping use of its
DB lock but does not turn the other existing entry points into a concurrent or
multi-tenant security boundary. Callbacks' transport timeouts and resource limits
remain the host's responsibility. This initial interface is synchronous.

## Dispatch failures, logging, and local simulator atomicity

**A tool or logging failure after dispatch is never automatically retried.** A tool
may already have sent an email, charged money, or mutated external state. A normal
tool exception is terminal `tool_error`, with failed input/output history retained.
Its external outcome is `uncertain`; inspect/reconcile it before starting another
action. A failure while storing output/audit or committing can be terminal
`action_error`/`audit_error`, likewise potentially uncertain. The in-memory session
retains the failure even if the DB audit could not be written. Database rollback
is not evidence of external rollback.

For a **local simulator only**, `action_transaction` may supply a trusted context
manager that snapshots/restores the simulator's state on an exception. It surrounds
dispatch, output logging and the DB commit; a tool's reported failure is raised
inside that context to trigger its rollback. The context manager must propagate
exceptions and have a non-failing successful exit. This coordinates the common
single-process mock/simulator case, not arbitrary remote systems or distributed
transactions. A normal propagated body failure reports rollback under this
trusted hook contract. A failing/suppressing hook exit is instead terminal and
uncertain; if the DB has already committed, its existing audit row is updated,
not duplicated. The library cannot verify arbitrary rollback code or repair a
broken transaction hook.

```python
from contextlib import contextmanager

@contextmanager
def atomic_simulator():
    before = list(sent)
    try:
        yield
    except Exception:
        sent[:] = before
        raise

# Pass action_transaction=atomic_simulator to RetryToolRunner(...).
```

Existing native engine limitations (including unsupported SQL/provenance cases)
remain limitations; this layer does not repair them or equate a technical failure
with a policy judgment. The API also does not make policy generation trustworthy,
prove complete policy coverage, or automatically enforce user-facing text unless
the host models that output as a protected tool sink.
