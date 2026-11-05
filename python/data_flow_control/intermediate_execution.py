"""Bounded automatic DuckDB propagation through Connection.execute.

The native catalog-backed lineage rewriter instruments SELECTs; this adapter
materializes policy states and executes effects while keeping hidden state out
of the logical schema. Unsupported SQL fails before effects.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import json

from .intermediate_policy import _from_registered
from .intermediates import (
    IntermediateError,
    PolicyIntermediateState,
    _quote,
    validate_transfer_type,
)
from .errors import RetryFailure, RetryViolation
from ._passant import PassantRewriteError, PyIntermediateSession

_STATE = "_dfc_intermediate_state"
_DEPS = "_dfc_intermediate_dependencies"
_KEY = "_dfc_intermediate_source_rowid"
_RESERVED = {_STATE, _DEPS, _KEY}


@dataclass
class IntermediateResult:
    """Materialized DB-API fetch result with row-aligned trusted states."""

    _rows: list[tuple]
    description: Any
    intermediates: tuple[tuple[PolicyIntermediateState, ...], ...]
    _offset: int = 0

    def fetchall(self):
        rows = self._rows[self._offset :]
        self._offset = len(self._rows)
        return rows

    def fetchone(self):
        rows = self.fetchmany(1)
        return rows[0] if rows else None

    def fetchmany(self, size=1):
        rows = self._rows[self._offset : self._offset + size]
        self._offset += len(rows)
        return rows


class IntermediateExecution:
    def __init__(self, connection):
        self.owner = connection
        self.db = connection.raw_connection
        self.policy_indices = tuple(connection.planner.inner.registered_policy_indices())
        self.policies = tuple(connection.planner.policy_by_index(i) for i in self.policy_indices)
        self.compiled = tuple(
            _from_registered(self.db, connection.planner, index, policy)
            for index, policy in zip(self.policy_indices, self.policies)
        )
        self.sources = {s.lower(): s for p in self.policies for s in p.sources}
        self.source_bindings = {name: self._source_binding(name) for name in self.sources}
        self.tracked: set[str] = set()
        self.session = PyIntermediateSession(
            [compiled.plan._native for compiled in self.compiled], self.sources
        )
        self._native_call("resolve_intermediate_rows", None, list(self.policy_indices), [])
        self.last_result: IntermediateResult | None = None
        for source in self.sources.values():
            if "rowid" in {c.lower() for c in self._columns(source)}:
                raise IntermediateError("A source column shadows DuckDB's contributor rowid")
        # Refuse a pre-existing physical schema which could spoof state columns.
        for (name,) in self.db.execute(
            "SELECT column_name FROM information_schema.columns"
        ).fetchall():
            if name.lower() in _RESERVED:
                raise IntermediateError(f"Reserved intermediate column already exists: {name}")

    def _source_binding(self, name):
        # Reject ambiguous names instead of guessing search-path precedence.
        # Include views so a temporary view cannot shadow a pinned base table.
        bindings = self.db.execute(
            "SELECT 'table', database_oid, schema_oid, table_oid FROM duckdb_tables() "
            "WHERE lower(table_name) = ? UNION ALL "
            "SELECT 'view', database_oid, schema_oid, view_oid FROM duckdb_views() "
            "WHERE lower(view_name) = ?",
            [name.lower(), name.lower()],
        ).fetchall()
        if len(bindings) != 1 or bindings[0][0] != "table":
            raise IntermediateError(
                f"Physical SOURCE binding for {name!r} must be one unambiguous base table"
            )
        return tuple(bindings[0][1:])

    def _validate_source_bindings(self):
        for name, binding in self.source_bindings.items():
            if self._source_binding(name) != binding:
                raise IntermediateError(f"Physical SOURCE binding changed for {name!r}")

    def _columns(self, table):
        return [
            r[0]
            for r in self.db.execute(f"DESCRIBE {_quote(table)}").fetchall()
            if r[0].lower() != _STATE
        ]

    def _native_call(self, method, *args):
        try:
            return getattr(self.owner.planner.inner, method)(*args)
        except PassantRewriteError as exc:
            raise IntermediateError(str(exc)) from exc

    def _sync_native(self):
        snapshot = self.owner._introspect_catalog()
        for (name,) in self.db.execute(
            "SELECT table_name FROM duckdb_tables() WHERE temporary AND NOT internal"
        ).fetchall():
            snapshot["tables"][name] = {"columns": self._columns(name)}
        snapshot["intermediate_bindings"] = {
            name: {
                "source": self.sources.get(name),
                "state_column": _STATE if name in self.tracked else None,
            }
            for name in self.sources.keys() | self.tracked
        }
        self.owner.planner.sync_catalog(snapshot)

    def _select(self, sql, sink=None, sink_columns=None, params=None):
        cursor = self.db.execute(sql, params)
        description = cursor.description[:-1]
        for column in description:
            if column[0].lower() in _RESERVED:
                raise IntermediateError("Dynamic projection exposed reserved intermediate metadata")
            validate_transfer_type(column[1])
        rows = cursor.fetchall()
        target_casts = None
        if sink_columns is not None:
            types = {r[0]: r[1] for r in self.db.execute(f"DESCRIBE {_quote(sink)}").fetchall()}
            for name in sink_columns:
                validate_transfer_type(types[name])
            target_casts = "SELECT " + ", ".join(f"CAST(? AS {types[n]})" for n in sink_columns)
        names = sink_columns or [c[0] for c in description]
        if len(names) != len(description) or len(set(names)) != len(names):
            raise IntermediateError("Output columns must have distinct names and match the sink")
        public_rows, states, verdicts = [], [], []
        applicable = set(self._native_call("intermediate_policy_indices", sink))
        for row in rows:
            state = self.session.row_states(self.db, row[-1] or [])
            public = tuple(row[:-1])
            if target_casts is not None:
                public = tuple(self.db.execute(target_casts, public).fetchone())
            output = dict(zip(names, public))
            public_rows.append(public)
            states.append(state)
            verdicts.append(
                [
                    compiled.evaluate(state[index], sink=output)
                    if policy_id in applicable
                    else None
                    for index, (policy_id, compiled) in enumerate(
                        zip(self.policy_indices, self.compiled)
                    )
                ]
            )
        outcome = json.loads(
            self._native_call(
                "resolve_intermediate_rows", sink, list(self.policy_indices), verdicts
            )
        )
        if outcome["kind"] == "kill":
            self.db.execute("SELECT passant_kill()")
        if outcome["kind"] == "retry":
            raise RetryViolation(
                [
                    RetryFailure(
                        f["policy_index"],
                        f["message"],
                        self.owner.planner.policy_by_index(f["policy_index"]),
                    )
                    for f in outcome["failures"]
                ]
            )
        indices = outcome["indices"]
        return IntermediateResult(
            [public_rows[i] for i in indices], description, tuple(states[i] for i in indices)
        )

    def execute(self, sql, params=None):
        self.last_result = None
        self._native_call("resolve_intermediate_rows", None, list(self.policy_indices), [])
        # Own the transaction so source capture, sink filtering and writes use
        # one snapshot. An existing external transaction fails BEGIN before work.
        self.db.execute("BEGIN TRANSACTION")
        checkpoint = self.session.checkpoint()
        old_tracked = self.tracked.copy()
        old_bindings = self.source_bindings.copy()
        try:
            self._validate_source_bindings()
            self._sync_native()
            plan = json.loads(self._native_call("plan_intermediate_statement", sql))
            result = self._execute(plan, params)
            self.db.execute("COMMIT")
        except Exception:
            self.db.execute("ROLLBACK")
            self.tracked = old_tracked
            self.source_bindings = old_bindings
            self.session.rollback(checkpoint)
            raise
        self.last_result = result
        self.owner._invalidate_catalog_cache()
        return result

    def _execute(self, plan, params):
        if plan["kind"] == "select":
            return self._select(plan["query_sql"], params=params)
        name = plan["target"]
        if plan["kind"] == "create":
            if plan["query_sql"] is None:
                self.db.execute(plan["ddl_sql"], params)
                return IntermediateResult([], None, ())
            result = self._select(plan["query_sql"], sink=name, params=params)
            columns = ", ".join(f"{_quote(c[0])} {c[1]}" for c in result.description)
            self.db.execute(
                f"CREATE {'TEMP ' if plan['temporary'] else ''}TABLE {_quote(name)} ({columns})"
            )
            columns = [c[0] for c in result.description]
        else:
            columns = plan["columns"]
            result = self._select(plan["query_sql"], sink=name, sink_columns=columns, params=params)
        self._persist(name, columns, result)
        return IntermediateResult(
            [(len(result._rows),)], [("Count", "BIGINT", None, None, None, None, None)], ()
        )

    def merge_rows(self, *rows):
        return self.session.merge_rows(self.db, rows)

    def store_rows(self, table, rows, intermediates):
        """Trusted tool-adapter bridge; SQL mapping has already selected inputs."""
        self._native_call("resolve_intermediate_rows", None, list(self.policy_indices), [])
        if not isinstance(table, str):
            raise IntermediateError("Output table must be a declared physical table name")
        columns = self._columns(table)
        rows, intermediates = list(rows), tuple(intermediates)
        if len(rows) != len(intermediates) or any(len(r) != len(columns) for r in rows):
            raise IntermediateError("Output rows and intermediate states must align")
        for states in intermediates:
            self.merge_rows(states)  # validate plan ownership before effects
        schema = [
            (r[0], r[1], None, None, None, None, None)
            for r in self.db.execute(f"DESCRIBE {_quote(table)}").fetchall()
            if r[0].lower() != _STATE
        ]
        for column in schema:
            validate_transfer_type(column[1])
        result = IntermediateResult(rows, schema, intermediates)
        self.db.execute("BEGIN TRANSACTION")
        checkpoint, old_tracked = self.session.checkpoint(), self.tracked.copy()
        old_bindings = self.source_bindings.copy()
        try:
            self._validate_source_bindings()
            self._persist(table, columns, result)
            self.db.execute("COMMIT")
        except Exception:
            self.db.execute("ROLLBACK")
            self.tracked = old_tracked
            self.source_bindings = old_bindings
            self.session.rollback(checkpoint)
            raise
        self.last_result = result
        self.owner._invalidate_catalog_cache()

    def _persist(self, table, columns, result):
        if table.lower() not in self.tracked:
            self.db.execute(f"ALTER TABLE {_quote(table)} ADD COLUMN {_quote(_STATE)} BIGINT")
            self.tracked.add(table.lower())
            if table.lower() in self.source_bindings:
                # Only the trusted observation bridge can reach this for a
                # declared source. ALTER installs our metadata and changes OID.
                self.source_bindings[table.lower()] = self._source_binding(table.lower())
        tokens = self.session.register_rows(result.intermediates)
        values = [(*row, token) for row, token in zip(result._rows, tokens)]
        if values:
            names = ", ".join(_quote(c) for c in [*columns, _STATE])
            placeholders = ", ".join("?" for _ in [*columns, _STATE])
            self.db.executemany(
                f"INSERT INTO {_quote(table)} ({names}) VALUES ({placeholders})", values
            )
