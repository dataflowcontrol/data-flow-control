"""Single-attempt tool dispatch with native PGN RETRY feedback."""

from __future__ import annotations

import copy
import hashlib
import inspect
import json
from collections.abc import Callable, Collection, Mapping
from contextlib import AbstractContextManager, nullcontext
from dataclasses import asdict
from enum import Enum
from typing import Any

import duckdb
import sqlglot
from sqlglot import exp

from data_flow_control.errors import RetryViolation
from data_flow_control.langchain.database import ToolUseDatabase
from data_flow_control.langchain.schema import quote_identifier
from data_flow_control.policy import Policy


def _plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Policy):
        return _plain(asdict(value))
    if isinstance(value, dict):
        return {k: _plain(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_plain(v) for v in value]
    return value


def _fingerprint(value: Any) -> str:
    return hashlib.sha256(json.dumps(_plain(value), sort_keys=True).encode()).hexdigest()


def policy_fingerprint(policy: Policy) -> str:
    """Identify an exact *registered* policy (registration normalizes expressions)."""
    return _fingerprint(policy)


class _Rejected(Exception):
    def __init__(self, category: str, message: str):
        super().__init__(message)
        self.category = category


# Deliberately small, deterministic SQL subset: no filesystem/network functions,
# custom UDFs, random/time/sequence functions, or table functions in agent SQL.
_PURE_FUNCTIONS = frozenset(
    {
        "ABS",
        "AND",
        "ARRAY",
        "CAST",
        "CEIL",
        "COALESCE",
        "CONCAT",
        "CONCAT_WS",
        "CONTAINS",
        "COUNT",
        "FLOOR",
        "GREATEST",
        "IF",
        "LEAST",
        "LENGTH",
        "LOWER",
        "MAX",
        "MIN",
        "NULLIF",
        "OR",
        "REPLACE",
        "ROUND",
        "SUBSTRING",
        "SUM",
        "TRIM",
        "TRY_CAST",
        "UPPER",
        "BOOL_AND",
        "BOOL_OR",
    }
)


class RetryToolRunner:
    """Check and dispatch one tool action, or return its native RETRY feedback.

    Each call to ``run`` or ``run_sql`` executes its proposed insertion once.
    There is no candidate query, replay, callback, or automatic retry loop. A host
    may submit a separately generated replacement using ``run_sql``.

    Use a dedicated database containing only caller-visible state. The host owns
    scheduling, retry limits, and model transport. Do not call this runner inside
    a user transaction: DuckDB rejects nested BEGIN and may abort earlier writes. ``readable_tables`` grants SQL
    access to all rows of each listed table; feedback itself contains schemas and
    explicitly supplied visible context, not automatically fetched source rows.

    ``action_transaction`` is an optional trusted local-simulator rollback hook,
    not a transaction over remote tool side effects. Tool/audit failures are never
    treated as policy rejections or automatically replayed.
    """

    def __init__(
        self,
        db: ToolUseDatabase,
        *,
        readable_tables: Collection[str] = (),
        require_sink_policy: bool = True,
        max_context_chars: int = 16000,
        action_transaction: Callable[[], AbstractContextManager] | None = None,
    ) -> None:
        if type(max_context_chars) is not int or max_context_chars < 0:
            raise ValueError("max_context_chars must be a nonnegative integer")
        self.db = db
        self.max_context_chars = max_context_chars
        self.require_sink_policy = require_sink_policy
        self.action_transaction = action_transaction
        self._policies = copy.deepcopy(db.conn.policies())
        self._policy_hash = _fingerprint(self._policies)
        self._schemas: dict[str, list[dict[str, str]]] = {}
        for table in readable_tables:
            if not isinstance(table, str) or not table or "." in table:
                raise ValueError("readable_tables must contain unqualified table names")
            rows = db._fetchall_raw(f"DESCRIBE {quote_identifier(table)}")
            self._schemas[table] = [{"name": row[0], "type": row[1]} for row in rows]
        self.sessions: list[dict[str, Any]] = []

    def _unchanged(self) -> bool:
        return _fingerprint(self.db.conn.policies()) == self._policy_hash

    def _ordinary_sql(self, tool: str, arguments: Mapping[str, Any]) -> str:
        spec = self.db.spec_by_name[tool]
        if not isinstance(arguments, Mapping) or set(arguments) - set(spec.input_schema):
            raise ValueError("Arguments must be a mapping of declared input columns")
        arguments = dict(arguments)
        missing = set(spec.input_schema) - set(arguments)
        if missing:
            parameters = inspect.signature(spec.call_sync).parameters
            for name in missing:
                parameter = parameters.get(name)
                if parameter is None or parameter.default is inspect.Parameter.empty:
                    raise ValueError(f"Missing argument {name!r}; no inspectable scalar default")
                arguments[name] = parameter.default
        if not arguments:
            raise ValueError("Tools with no declared arguments are not supported")
        # Literals are deliberately SELECT expressions, not VALUES: all calls
        # must take the native provenance-rewriting path, including direct calls.
        expressions = []
        for value in arguments.values():
            if not isinstance(value, (str, int, float, bool, type(None))):
                raise TypeError("Arguments must be SQL scalar values")
            expressions.append(exp.convert(value))
        return exp.Insert(
            this=exp.Schema(
                this=exp.to_table(self.db.input_table_by_tool[tool]),
                expressions=[exp.to_identifier(name) for name in arguments],
            ),
            expression=exp.Select(expressions=expressions),
        ).sql(dialect="duckdb")

    def _validate(self, sql: str, sink: str) -> str:
        statements = [
            statement for statement in sqlglot.parse(sql, read="duckdb") if statement is not None
        ]
        if len(statements) != 1:
            raise ValueError("Exactly one SQL statement is required")
        tree = statements[0]
        if not isinstance(tree, exp.Insert) or not isinstance(tree.this, exp.Schema):
            raise ValueError("Retry must be one INSERT with explicit argument columns")
        target = tree.this.this
        if (
            not isinstance(target, exp.Table)
            or target.name.lower() != sink.lower()
            or target.db
            or target.catalog
        ):
            raise ValueError("Retry must target the original unqualified tool input relation")
        if {key for key, value in tree.args.items() if value} - {"this", "expression"}:
            raise ValueError("INSERT modifiers are not allowed")
        tool = next(name for name, table in self.db.input_table_by_tool.items() if table == sink)
        columns = [column.name.lower() for column in tree.this.expressions]
        declared = {name.lower() for name in self.db.spec_by_name[tool].input_schema}
        if (
            len(columns) != len(set(columns))
            or set(columns) != declared
            or not all(isinstance(column, exp.Identifier) for column in tree.this.expressions)
        ):
            raise ValueError("INSERT must specify every declared user argument exactly once")
        query = tree.expression
        if not isinstance(query, exp.Select):
            raise ValueError("Retry requires INSERT SELECT (not VALUES or a set operation)")
        if {key for key, value in query.args.items() if value} - {
            "expressions",
            "from_",
            "joins",
            "where",
            "group",
            "having",
            "order",
            "limit",
            "offset",
            "distinct",
        }:
            raise ValueError("Unsupported SELECT clause")
        if query.args.get("with_") or any(query.find_all(exp.Subquery)):
            raise ValueError("CTEs and subqueries are not supported in retry SQL")
        allowed = {name.lower() for name in self._schemas}
        for table in query.find_all(exp.Table):
            if (
                not isinstance(table.this, exp.Identifier)
                or table.db
                or table.catalog
                or table.name.lower() not in allowed
            ):
                raise ValueError("SQL may only read explicitly authorized local tables")
        for node in query.walk():
            if isinstance(node, exp.Func):
                name = node.name.upper() if isinstance(node, exp.Anonymous) else node.sql_name()
                if name not in _PURE_FUNCTIONS:
                    raise ValueError(f"Function {name!r} is outside the deterministic SQL subset")
        return tree.sql(dialect="duckdb")

    def _attempt(self, sql: str, sink: str, tool: str) -> dict[str, Any]:
        normalized = self._validate(sql, sink)
        sql_call_id = self.db._next_sql_call_id()
        event: dict[str, Any] = {"sql": normalized, "sql_call_id": sql_call_id}
        stage = "begin"
        dispatched = False
        transaction = False
        committed = False
        body_error: Exception | None = None
        try:
            self.db._execute_raw("BEGIN TRANSACTION")
            transaction = True
            before = self.db.snapshot_pending_input_ids()
            stage = "guard"
            self.db.conn.execute(normalized)
            new_rows = self.db.find_new_pending_rows(before)
            event["inserted_count"] = len(new_rows)
            if len(new_rows) > 1:
                raise _Rejected("invalid_sql", "One action may insert at most one input row")
            if not new_rows:
                event.update(status="no_action", tool_calls_executed=0)
            else:
                resolved_tool, input_id = new_rows[0]
                if resolved_tool != tool:
                    raise _Rejected("invariant_error", "Inserted row resolved to a different tool")
                stage = "dispatch"
                with self.action_transaction() if self.action_transaction else nullcontext():
                    try:
                        if not self.db.claim_input_row(tool, input_id):
                            raise _Rejected(
                                "invariant_error", "Could not claim the checked input row"
                            )
                        dispatched = True
                        status, output, output_id = self.db.execute_tool_for_input(
                            tool, input_id, include_nulls=True
                        )
                        event.update(
                            status="executed" if status == "succeeded" else "tool_error",
                            tool_calls_executed=1,
                            input_id=input_id,
                            output_id=output_id,
                            output=output,
                            external_outcome="completed" if status == "succeeded" else "uncertain",
                        )
                        if status != "succeeded" and self.action_transaction:
                            raise _Rejected(
                                "tool_error", "Tool failed; local action transaction rolled back"
                            )
                        stage = "logging"
                        self.db.log_sql_call(
                            sql_call_id=sql_call_id,
                            sql=normalized,
                            row_count_inserted=1,
                            tool_calls_executed=1,
                            error=None if status == "succeeded" else "Tool failed",
                        )
                        self.db._execute_raw("COMMIT")
                        transaction = False
                        committed = True
                    except Exception as exc:
                        body_error = exc
                        raise
                if body_error is not None:
                    raise _Rejected(
                        "action_error", "action_transaction suppressed an action failure"
                    )
            if transaction:
                self.db.log_sql_call(
                    sql_call_id=sql_call_id,
                    sql=normalized,
                    row_count_inserted=0,
                    tool_calls_executed=0,
                )
                self.db._execute_raw("COMMIT")
                transaction = False
                committed = True
            return event
        except Exception as exc:
            if transaction:
                try:
                    self.db._execute_raw("ROLLBACK")
                except Exception as rollback_exc:
                    event["rollback_error"] = str(rollback_exc)
            if isinstance(exc, RetryViolation) and stage == "guard":
                category = "retry"
                event.update(
                    message=exc.message,
                    messages=list(exc.messages),
                    policy_index=exc.policy_index,
                    failed_policy=_plain(exc.policy),
                    failures=[
                        {
                            "policy_index": failure.policy_index,
                            "message": failure.message,
                            "policy": _plain(failure.policy),
                        }
                        for failure in exc.failures
                    ],
                )
            elif isinstance(exc, _Rejected):
                category = exc.category
            elif (
                stage == "guard"
                and isinstance(exc, duckdb.InvalidInputException)
                and "KILLing due to dfc policy violation" in str(exc)
            ):
                category = "hard_stop"
            else:
                category = "action_error" if dispatched else "engine_error"
            event.update(
                status=category,
                error=str(exc),
                error_type=type(exc).__name__,
                tool_calls_executed=int(dispatched),
                external_outcome="rolled_back"
                if dispatched and self.action_transaction and body_error is exc and not committed
                else "uncertain"
                if dispatched
                else "not_dispatched",
            )
            event["database_committed"] = committed
            try:
                if committed:
                    # The hook's successful-exit code failed after DB commit.
                    # Preserve the committed history; never append a duplicate ID.
                    self.db._execute_raw(
                        "UPDATE __passant_tool_sql_calls SET error = ? WHERE sql_call_id = ?",
                        [f"{category}: {exc}", sql_call_id],
                    )
                else:
                    self.db.log_sql_call(
                        sql_call_id=sql_call_id,
                        sql=normalized,
                        row_count_inserted=0,
                        tool_calls_executed=int(dispatched),
                        error=f"{category}: {exc}",
                    )
            except Exception as log_exc:
                event.update(status="audit_error", audit_error=str(log_exc))
            return event

    def run(
        self,
        tool: str,
        arguments: Mapping[str, Any],
        *,
        context: Mapping[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Check an ordinary call once; return success or native rejection feedback."""
        return self._run(tool, arguments=arguments, context=context)

    def run_sql(
        self,
        tool: str,
        sql: str,
        *,
        context: Mapping[str, Any] | None = None,
    ) -> dict[str, Any]:
        """Check one host-submitted same-tool INSERT SELECT, without replaying it."""
        return self._run(tool, sql=sql, context=context)

    def _run(self, tool, *, arguments=None, sql=None, context=None):
        if not self.db._lock.acquire(blocking=False):
            raise RuntimeError("Tool database is already in use")
        try:
            return self._run_locked(tool, arguments=arguments, sql=sql, context=context)
        finally:
            self.db._lock.release()

    def _run_locked(self, tool, *, arguments, sql, context):
        session = {
            "requested_tool": tool,
            "policy_set_fingerprint": self._policy_hash,
            "attempts": [],
        }
        if arguments is not None:
            session["original_arguments"] = copy.deepcopy(arguments)
        self.sessions.append(session)

        def finish(status, **extra):
            session.update(status=status, **extra)
            return copy.deepcopy(session)

        if not self._unchanged():
            return finish("policy_changed")
        try:
            sink = self.db.input_table_by_tool[tool]
            if sql is None:
                sql = self._ordinary_sql(tool, arguments)
            if not isinstance(sql, str):
                raise TypeError("SQL must be a string")
            encoded_context = json.dumps(dict(context or {}))
            if len(encoded_context) > self.max_context_chars:
                raise ValueError("Host context exceeds max_context_chars; redact or truncate it")
            visible_context = json.loads(encoded_context)
        except (ValueError, TypeError, KeyError) as exc:
            return finish("invalid_request", error=str(exc))
        session.update(resolved_tool=tool, required_sink=sink)
        if self.require_sink_policy and not any(
            p.sink and p.sink.lower() == sink.lower() for p in self._policies
        ):
            return finish(
                "coverage_gap", error="No registered policy covers the resolved tool sink"
            )
        try:
            event = self._attempt(sql, sink, tool)
        except (ValueError, sqlglot.errors.ParseError) as exc:
            event = {
                "status": "invalid_sql",
                "error": str(exc),
                "sql": sql,
                "tool_calls_executed": 0,
            }
        session["attempts"].append(event)
        if event["status"] == "retry":
            return finish(
                "retry",
                result=event,
                message=event["message"],
                messages=event["messages"],
                failures=event["failures"],
                feedback={
                    "category": "policy_denial",
                    "message": event["message"],
                    "messages": event["messages"],
                    "failures": event["failures"],
                    "policy_index": event["policy_index"],
                    "failed_policy": event["failed_policy"],
                    "required_sink": sink,
                    "input_schema": copy.deepcopy(self.db.spec_by_name[tool].input_schema),
                    "readable_source_schemas": copy.deepcopy(self._schemas),
                    "context": visible_context,
                },
            )
        return finish(event["status"], result=event)
