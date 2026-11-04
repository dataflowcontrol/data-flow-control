"""Bounded automatic DuckDB propagation through Connection.execute.

The SQL rewriter has no persisted intermediate schema yet. This opt-in adapter
instruments direct SELECTs and materialized relations while keeping hidden state
out of the logical schema. Unsupported SQL fails before effects.
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

_STATE = "_dfc_intermediate_state"
_DEPS = "_dfc_intermediate_dependencies"
_KEY = "_dfc_intermediate_source_rowid"
_RESERVED = {_STATE, _DEPS, _KEY}
_EMPTY = "[]::STRUCT(t VARCHAR, r BIGINT)[]"


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
        self.policies = tuple(connection.policies())
        if any(p.on_fail not in (Resolution.REMOVE, Resolution.KILL) for p in self.policies):
            raise IntermediateError(
                "Automatic intermediate execution currently supports REMOVE and KILL only"
            )
        self.compiled = tuple(compile_policy_intermediates(self.db, p) for p in self.policies)
        self.sources = {s.lower(): s for p in self.policies for s in p.sources}
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
            raise IntermediateError("Automatic propagation requires a direct SELECT")
        if any(
            isinstance(n, (exp.Subquery, exp.With, exp.Window))
            or isinstance(n, exp.Query)
            and n is not query
            for n in query.walk()
        ):
            raise IntermediateError(
                "Nested queries, CTEs, windows and set operations are unsupported"
            )
        if query.args.get("into") or query.args.get("locks"):
            raise IntermediateError("SELECT INTO and locking clauses are unsupported")
        if query.args.get("distinct") or query.args.get("qualify"):
            raise IntermediateError("DISTINCT and QUALIFY require an intermediate merge rewrite")
        if query.args.get("having"):
            raise IntermediateError("HAVING is not supported by automatic propagation yet")
        if any(isinstance(n, exp.Columns) for n in query.walk()):
            raise IntermediateError("Dynamic COLUMNS projections are unsupported with hidden state")
        tables = list(query.find_all(exp.Table))
        bindings = []
        dependencies = []
        for table in tables:
            name = self._table(table)
            alias = table.alias_or_name
            bindings.append((alias, self._columns(name)))
            if name.lower() in self.sources:
                dependencies.append(
                    f"[{{'t': 'source:{name.lower()}', 'r': {_quote(alias)}.rowid}}]"
                )
            if name.lower() in self.tracked:
                dependencies.append(f"[{{'t': 'state', 'r': {_quote(alias)}.{_quote(_STATE)}}}]")
        aliases = {a.lower() for a, _ in bindings}
        if any(not c.table and c.name.lower() in aliases for c in query.find_all(exp.Column)):
            raise IntermediateError("Whole-row table references are unsupported with hidden state")
        for join in query.args.get("joins") or []:
            if join.args.get("side") or join.args.get("method") or join.args.get("using"):
                raise IntermediateError(
                    "Nullable, NATURAL and USING joins need a presence-aware rewrite"
                )
            if join.args.get("kind") not in (None, "", "INNER", "CROSS"):
                raise IntermediateError("Only inner and cross joins are supported")
        # Expansion happens against logical columns, before hidden metadata is
        # appended. Even SELECT alias.* never exposes the state token.
        projection = []
        for item in query.expressions:
            if isinstance(item, exp.Star) or (isinstance(item, exp.Column) and item.is_star):
                star = item.this if isinstance(item, exp.Column) else item
                if any(star.args.get(k) for k in ("except_", "replace", "rename")):
                    raise IntermediateError("Modified stars are unsupported")
                qualifier = item.table if isinstance(item, exp.Column) else None
                matched = [(a, cols) for a, cols in bindings if qualifier is None or a == qualifier]
                if not matched:
                    raise IntermediateError("Star has no physical table binding")
                projection.extend(
                    exp.column(c, table=a, quoted=True) for a, cols in matched for c in cols
                )
            else:
                projection.append(item.copy())
        order = query.args.get("order")
        if order:
            for ordered in order.expressions:
                value = ordered.this
                if (
                    isinstance(value, exp.Literal)
                    and not value.is_string
                    and value.this.isdigit()
                    and int(value.this) > len(projection)
                ):
                    raise IntermediateError("ORDER BY position exceeds the logical projection")
        annotated = query.copy()
        annotated.set("expressions", projection)
        deps = f"list_concat({', '.join(dependencies)})" if dependencies else _EMPTY
        is_group = query.args.get("group") is not None or any(
            isinstance(n, exp.AggFunc) for item in query.expressions for n in item.walk()
        )
        if is_group:
            if query.args.get("group") and query.args["group"].args.get("all"):
                raise IntermediateError("GROUP BY ALL is unsupported")
            deps = f"COALESCE(list_distinct(flatten(list({deps}))), {_EMPTY})"
        annotated.select(sqlglot.parse_one(deps, read="duckdb").as_(_DEPS), append=True, copy=False)
        return annotated.sql(dialect="duckdb")

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
        accepted, states = [], []
        for row in rows:
            state = self._row_states(row[-1])
            public = tuple(row[:-1])
            if target_casts is not None:
                public = tuple(self.db.execute(target_casts, public).fetchone())
            output = dict(zip(names, public))
            keep = True
            for index, (policy, compiled) in enumerate(zip(self.policies, self.compiled)):
                if policy.sink and (sink is None or policy.sink.lower() != sink.lower()):
                    continue
                verdict = compiled.evaluate(state[index], sink=output)
                if verdict is False:
                    if policy.on_fail is Resolution.KILL:
                        self.db.execute("SELECT passant_kill()")
                    keep = False
            if keep:
                accepted.append(public)
                states.append(state)
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
        try:
            result = self._execute(statement, params)
            self.db.execute("COMMIT")
        except Exception:
            self.db.execute("ROLLBACK")
            self.tracked = old_tracked
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
        try:
            self._persist(table, columns, result)
            self.db.execute("COMMIT")
        except Exception:
            self.db.execute("ROLLBACK")
            self.tracked = old_tracked
            self.states = {k: v for k, v in self.states.items() if k < old_token}
            self.next_token = old_token
            raise
        self.last_result = result
        self.owner._invalidate_catalog_cache()

    def _persist(self, table, columns, result):
        if table.lower() not in self.tracked:
            self.db.execute(f"ALTER TABLE {_quote(table)} ADD COLUMN {_quote(_STATE)} BIGINT")
            self.tracked.add(table.lower())
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
