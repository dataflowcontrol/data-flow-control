"""Bounded automatic DuckDB propagation through Connection.execute.

The native catalog-backed lineage rewriter instruments SELECTs; this adapter
materializes policy states and executes effects while keeping hidden state out
of the logical schema. Unsupported SQL fails before effects.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import sqlglot
from sqlglot import exp

from .intermediate_policy import compile_policy_intermediates
from .intermediates import (
    IntermediateError,
    PolicyIntermediateState,
    _quote,
    validate_transfer_type,
)
from .policy import Resolution
from .errors import RetryFailure, RetryViolation
from ._passant import PassantRewriteError

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
        if any(p.on_fail not in (Resolution.REMOVE, Resolution.KILL, Resolution.RETRY) for p in self.policies):
            raise IntermediateError(
                "Automatic intermediate execution currently supports REMOVE, KILL and RETRY only"
            )
        self.compiled = tuple(compile_policy_intermediates(self.db, p) for p in self.policies)
        self.sources = {s.lower(): s for p in self.policies for s in p.sources}
        self.source_bindings = {name: self._source_binding(name) for name in self.sources}
        self.tracked: set[str] = set()
        self.states: dict[int, tuple[PolicyIntermediateState, ...]] = {}
        self.next_token = 1
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

    @staticmethod
    def _table(table):
        if not isinstance(table, exp.Table) or table.db or table.catalog:
            raise IntermediateError(
                "Automatic propagation requires unqualified physical table names"
            )
        return table.name

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

    def _ordinary_target(self, table):
        name = self._table(table)
        if name.lower() in self.sources:
            raise IntermediateError(
                "Ordinary SQL cannot write or shadow a declared SOURCE relation; "
                "authoritative observations must come from the trusted harness"
            )
        return name

    def _columns(self, table):
        return [
            r[0]
            for r in self.db.execute(f"DESCRIBE {_quote(table)}").fetchall()
            if r[0].lower() != _STATE
        ]

    def _instrument(self, query):
        if not isinstance(query, exp.Select):
            raise IntermediateError("Automatic propagation requires a SELECT")
        snapshot = self.owner._introspect_catalog()
        snapshot["intermediate_bindings"] = {
            name: {
                "source": self.sources.get(name),
                "state_column": _STATE if name in self.tracked else None,
            }
            for name in self.sources.keys() | self.tracked
        }
        self.owner.planner.sync_catalog(snapshot)
        try:
            return self.owner.planner.inner.transform_intermediate_query(query.sql(dialect="duckdb"))
        except PassantRewriteError as exc:
            raise IntermediateError(str(exc)) from exc

    def _row_states(self, dependencies):
        inherited = []
        direct: dict[str, set[int]] = {}
        for dependency in dependencies or []:
            if dependency["r"] is None:
                continue
            if dependency["t"] == "state":
                token = dependency["r"]
                if token not in self.states:
                    raise IntermediateError("Unknown persisted intermediate state")
                inherited.append(self.states[token])
            else:
                direct.setdefault(dependency["t"][7:], set()).add(dependency["r"])
        result = []
        for index, compiled in enumerate(self.compiled):
            parts = [states[index] for states in inherited]
            for source, identities in direct.items():
                canonical = self.sources[source]
                if canonical not in compiled.plan.sources:
                    continue
                # Rowids are database-generated integers, not SQL supplied by the
                # caller. No attribute values are reconstructed from old states.
                ids = ", ".join(str(int(n)) for n in sorted(identities))
                parts.append(
                    compiled.plan.capture(
                        canonical,
                        f"SELECT *, rowid AS {_quote(_KEY)} FROM {_quote(canonical)} WHERE rowid IN ({ids})",
                        identity_columns=[_KEY],
                    )
                )
            result.append(compiled.plan.merge(*parts))
        return tuple(result)

    def _select(self, query, sink=None, sink_columns=None, params=None):
        sql = self._instrument(query)
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
        accepted, states, failures = [], [], []
        for row in rows:
            state = self._row_states(row[-1])
            public = tuple(row[:-1])
            if target_casts is not None:
                public = tuple(self.db.execute(target_casts, public).fetchone())
            output = dict(zip(names, public))
            keep = True
            row_failures = []
            for index, (policy, compiled) in enumerate(zip(self.policies, self.compiled)):
                if policy.sink and (sink is None or policy.sink.lower() != sink.lower()):
                    continue
                verdict = compiled.evaluate(state[index], sink=output)
                if verdict is False:
                    if policy.on_fail is Resolution.KILL:
                        self.db.execute("SELECT passant_kill()")
                    if policy.on_fail is Resolution.REMOVE:
                        keep = False
                    elif policy.on_fail is Resolution.RETRY:
                        row_failures.append(
                            RetryFailure(self.policy_indices[index], policy.retry_message, policy)
                        )
            if keep:
                failures.extend(row_failures)
                accepted.append(public)
                states.append(state)
        if failures:
            raise RetryViolation(failures)
        return IntermediateResult(accepted, description, tuple(states))

    def execute(self, sql, params=None):
        self.last_result = None
        statements = sqlglot.parse(sql, read="duckdb")
        if len(statements) != 1:
            raise IntermediateError("Automatic propagation requires one statement per execute")
        statement = statements[0]
        if any(
            isinstance(n, exp.Identifier) and n.name.lower() in _RESERVED for n in statement.walk()
        ):
            raise IntermediateError("Intermediate metadata columns are reserved")
        if tuple(self.owner.policies()) != self.policies:
            raise IntermediateError("Policy changes require a new intermediate execution session")
        # Own the transaction so source capture, sink filtering and writes use
        # one snapshot. An existing external transaction fails BEGIN before work.
        self.db.execute("BEGIN TRANSACTION")
        old_token = self.next_token
        old_tracked = self.tracked.copy()
        old_bindings = self.source_bindings.copy()
        try:
            self._validate_source_bindings()
            result = self._execute(statement, params)
            self.db.execute("COMMIT")
        except Exception:
            self.db.execute("ROLLBACK")
            self.tracked = old_tracked
            self.source_bindings = old_bindings
            self.states = {k: v for k, v in self.states.items() if k < old_token}
            self.next_token = old_token
            raise
        self.last_result = result
        self.owner._invalidate_catalog_cache()
        return result

    def _execute(self, statement, params):
        if isinstance(statement, exp.Select):
            return self._select(statement, params=params)
        if isinstance(statement, exp.Create) and statement.kind == "TABLE":
            if statement.args.get("replace") or statement.args.get("exists"):
                raise IntermediateError("CREATE OR REPLACE / IF NOT EXISTS is unsupported")
            target = statement.this
            name = self._ordinary_target(target.this if isinstance(target, exp.Schema) else target)
            if statement.expression is None:
                self.db.execute(statement.sql(dialect="duckdb"), params)
                return IntermediateResult([], None, ())
            if isinstance(target, exp.Schema):
                raise IntermediateError("CTAS column lists are unsupported")
            properties = (statement.args.get("properties") or exp.Properties()).expressions
            if any(not isinstance(p, exp.TemporaryProperty) for p in properties):
                raise IntermediateError("CTAS supports only the TEMPORARY table property")
            result = self._select(statement.expression, sink=name, params=params)
            columns = ", ".join(f"{_quote(c[0])} {c[1]}" for c in result.description)
            temporary = any(
                isinstance(p, exp.TemporaryProperty)
                for p in (statement.args.get("properties") or exp.Properties()).expressions
            )
            self.db.execute(
                f"CREATE {'TEMP ' if temporary else ''}TABLE {_quote(name)} ({columns})"
            )
            self._persist(name, [c[0] for c in result.description], result)
            return IntermediateResult(
                [(len(result._rows),)], [("Count", "BIGINT", None, None, None, None, None)], ()
            )
        if isinstance(statement, exp.Insert):
            if (
                statement.args.get("returning")
                or statement.args.get("conflict")
                or statement.args.get("overwrite")
                or statement.args.get("by_name")
                or statement.args.get("alternative")
                or statement.args.get("partition")
            ):
                raise IntermediateError("INSERT RETURNING/conflict/overwrite is unsupported")
            target = statement.this
            columns = None
            if isinstance(target, exp.Schema):
                columns = [c.name for c in target.expressions]
                target = target.this
            name = self._ordinary_target(target)
            columns = columns or self._columns(name)
            if set(columns) != set(self._columns(name)):
                raise IntermediateError(
                    "Intermediate INSERT must supply every logical target column"
                )
            result = self._select(
                statement.expression, sink=name, sink_columns=columns, params=params
            )
            self._persist(name, columns, result)
            return IntermediateResult(
                [(len(result._rows),)], [("Count", "BIGINT", None, None, None, None, None)], ()
            )
        raise IntermediateError(
            "Automatic propagation supports SELECT, CREATE TABLE, CTAS and INSERT SELECT only; "
            "other statements cannot silently discard or mutate provenance"
        )

    def merge_rows(self, *rows):
        if any(len(row) != len(self.compiled) for row in rows):
            raise IntermediateError("Intermediate row has a different policy set")
        return tuple(
            compiled.plan.merge(*(row[n] for row in rows))
            for n, compiled in enumerate(self.compiled)
        )

    def store_rows(self, table, rows, intermediates):
        """Trusted tool-adapter bridge; SQL mapping has already selected inputs."""
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
        old_token, old_tracked = self.next_token, self.tracked.copy()
        old_bindings = self.source_bindings.copy()
        try:
            self._validate_source_bindings()
            self._persist(table, columns, result)
            self.db.execute("COMMIT")
        except Exception:
            self.db.execute("ROLLBACK")
            self.tracked = old_tracked
            self.source_bindings = old_bindings
            self.states = {k: v for k, v in self.states.items() if k < old_token}
            self.next_token = old_token
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
        values = []
        for row, state in zip(result._rows, result.intermediates):
            token = self.next_token
            self.next_token += 1
            self.states[token] = state
            values.append((*row, token))
        if values:
            names = ", ".join(_quote(c) for c in [*columns, _STATE])
            placeholders = ", ".join("?" for _ in [*columns, _STATE])
            self.db.executemany(
                f"INSERT INTO {_quote(table)} ({names}) VALUES ({placeholders})", values
            )
