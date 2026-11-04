"""Trusted, DuckDB-backed policy intermediate states.

This API is a boundary for relational/tool provenance adapters. It does not infer
which input rows contributed to an output, or change Connection's SQL rewriter.
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
import re
from typing import Any

import sqlglot
from sqlglot import exp


def _quote(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


class IntermediateError(ValueError):
    """An intermediate cannot be computed without losing policy semantics."""


def validate_transfer_type(sql_type: str, *, identity: bool = False) -> None:
    """Fail before Python conversion when SQL equality/precision is not preserved."""
    sql_type = str(sql_type).upper()
    scalar_types = {
        "BOOLEAN",
        "TINYINT",
        "SMALLINT",
        "INTEGER",
        "BIGINT",
        "HUGEINT",
        "UTINYINT",
        "USMALLINT",
        "UINTEGER",
        "UBIGINT",
        "UHUGEINT",
        "FLOAT",
        "REAL",
        "DOUBLE",
        "VARCHAR",
        "BLOB",
    }
    if (
        sql_type not in scalar_types
        and not re.fullmatch(r"DECIMAL\(\d+,\d+\)", sql_type)
        or identity
        and sql_type in {"FLOAT", "REAL", "DOUBLE"}
    ):
        raise IntermediateError(
            f"Lossless intermediate transfer is not established for {sql_type}; "
            "use a lossless SQL scalar encoding or an integer contributor identity"
        )


@dataclass(frozen=True)
class Intermediate:
    """One source-local expression, evaluated before its aggregate.

    ``unique`` retains zero/one/conflict among non-NULL values, matching SQL
    COUNT(DISTINCT). ``avg`` retains sum and non-NULL count, not an average of
    averages. Source names must match the trusted provenance adapter's names.
    """

    name: str
    source: str
    expression: str
    aggregate: str = "unique"

    def __post_init__(self) -> None:
        if self.aggregate not in {
            "unique",
            "min",
            "max",
            "bool_and",
            "bool_or",
            "sum",
            "count",
            "avg",
        }:
            raise IntermediateError(f"Unsupported intermediate aggregate: {self.aggregate}")
        parsed = sqlglot.parse_one(self.expression, read="duckdb")
        if any(isinstance(n, (exp.AggFunc, exp.Query, exp.Window)) for n in parsed.walk()):
            raise IntermediateError("Intermediate expressions must be contributor-local scalars")
        if isinstance(parsed, exp.Star) and self.aggregate != "count":
            raise IntermediateError("Only COUNT supports *")

    @property
    def idempotent(self) -> bool:
        return self.aggregate in {"unique", "min", "max", "bool_and", "bool_or"}


@dataclass(frozen=True)
class _SourceState:
    source: str
    identities: frozenset[tuple]
    values: tuple[tuple[Any, ...], ...]


@dataclass(frozen=True)
class PolicyIntermediateState:
    """Opaque state: copy whole for projection/fan-out; merge for joins/groups.

    Identities are overlap metadata, never a store of contributor attributes.
    Keep this object in trusted harness state, outside model-visible SQL tables.
    """

    _owner: object
    _sources: tuple[_SourceState, ...]


class PolicyIntermediates:
    """Capture and merge multiple expressions for a policy on one DB snapshot.

    ``connection`` is a raw DuckDB connection, not a policy-aware Connection.
    Source queries, expressions and identity columns are trusted declarations.
    Every identity must denote one immutable source contributor for this plan's
    lifetime. Separate equal-valued contributors need separate identities.
    """

    def __init__(
        self,
        connection: Any,
        intermediates: Iterable[Intermediate],
        *,
        sources: Iterable[str] | None = None,
        required_sources: Iterable[str] = (),
    ) -> None:
        self.connection = connection
        self.intermediates = tuple(intermediates)
        self.sources = frozenset(
            sources if sources is not None else (i.source for i in self.intermediates)
        )
        self.required_sources = frozenset(required_sources)
        if not self.required_sources <= self.sources:
            raise IntermediateError("Required sources must be declared sources")
        if any(i.source not in self.sources for i in self.intermediates):
            raise IntermediateError("Every intermediate must belong to a declared source")
        names = [i.name for i in self.intermediates]
        exposed = names + [
            i.name + "__cardinality" for i in self.intermediates if i.aggregate == "unique"
        ]
        if len(exposed) != len(set(exposed)):
            raise IntermediateError("Intermediate names and cardinality columns must be unique")
        self._owner = object()

    def empty(self) -> PolicyIntermediateState:
        return PolicyIntermediateState(self._owner, ())

    def _specs(self, source: str) -> tuple[Intermediate, ...]:
        return tuple(i for i in self.intermediates if i.source == source)

    def _validate(self, state: PolicyIntermediateState) -> None:
        if state._owner is not self._owner:
            raise IntermediateError("State belongs to another policy plan or snapshot")

    def capture(
        self, source: str, query: str, *, identity_columns: Iterable[str]
    ) -> PolicyIntermediateState:
        """Aggregate one contributor relation (for one output row).

        A join may repeat a contributor; identical keys are counted once. A key
        with inconsistent expression values is rejected. The query must expose
        the source's columns and stable, non-NULL contributor identity columns.
        Filtering/grouping and output correspondence belong to the caller.
        """
        if source not in self.sources:
            raise IntermediateError(f"Undeclared source: {source}")
        statements = sqlglot.parse(query, read="duckdb")
        if len(statements) != 1 or not isinstance(statements[0], exp.Query):
            raise IntermediateError("Capture requires one SELECT query")
        query = statements[0].sql(dialect="duckdb")
        keys = tuple(identity_columns)
        if not keys or len(keys) != len(set(keys)):
            raise IntermediateError("Provide distinct contributor identity columns")
        specs = self._specs(source)
        key_sql = [f"{_quote(k)} AS k{n}" for n, k in enumerate(keys)]
        expr_sql = [
            f"{'1' if i.expression.strip() == '*' else i.expression} AS v{n}"
            for n, i in enumerate(specs)
        ]
        # Python datetime truncates TIMESTAMP_NS, and ENUM/container/domain
        # values can lose SQL equality/ordering when rebound as parameters.
        # Until a typed codec exists, reject these before fetching either IDs
        # or metric values. Checking only aggregate outputs misses COUNT inputs.
        transfer_sql = f"SELECT {', '.join(key_sql + expr_sql)} FROM ({query}) AS {_quote(source)}"
        for n, column in enumerate(
            self.connection.execute(f"DESCRIBE ({transfer_sql})").fetchall()
        ):
            validate_transfer_type(column[1], identity=n < len(keys))
        row_key = "ROW(" + ", ".join(f"k{n}" for n in range(len(keys))) + ")"
        null_key = " OR ".join(f"k{n} IS NULL" for n in range(len(keys)))
        columns = [
            f"LIST({row_key})",
            f"COUNT(*) <> COUNT(DISTINCT {row_key})",
            f"COALESCE(BOOL_OR({null_key}), FALSE)",
        ]
        for n, spec in enumerate(specs):
            if spec.aggregate == "unique":
                columns += [f"LEAST(COUNT(DISTINCT v{n}), 2)", f"MIN(v{n})"]
            elif spec.aggregate == "avg":
                columns += [f"SUM(v{n})", f"COUNT(v{n})"]
            else:
                columns += [f"{spec.aggregate}(v{n})"]
        # MATERIALIZED ensures each expression is evaluated once; DISTINCT
        # removes join multiplicity before any contributor aggregate is taken.
        sql = (
            f"WITH raw AS MATERIALIZED (SELECT {', '.join(key_sql + expr_sql)} "
            f"FROM ({query}) AS {_quote(source)}), "
            f"contributors AS (SELECT DISTINCT * FROM raw) "
            f"SELECT {', '.join(columns)} FROM contributors"
        )
        row = self.connection.execute(sql).fetchone()
        if row[1]:
            raise IntermediateError("One contributor identity has inconsistent expression values")
        if row[2]:
            raise IntermediateError("Contributor identities must not contain NULL")
        try:
            identities = frozenset(tuple(key) for key in (row[0] or []))
        except TypeError as error:
            raise IntermediateError(
                "Contributor identities must have hashable scalar fields"
            ) from error
        values, offset = [], 3
        for spec in specs:
            width = 2 if spec.aggregate in {"unique", "avg"} else 1
            values.append(tuple(row[offset : offset + width]))
            offset += width
        return PolicyIntermediateState(
            self._owner, (_SourceState(source, identities, tuple(values)),)
        )

    def merge(self, *states: PolicyIntermediateState) -> PolicyIntermediateState:
        """Union input provenance for a join, grouping, DISTINCT or tool output.

        Projection, selection and fan-out reuse the same state unchanged. No
        operation can split an existing state back into its contributors.
        """
        by_source: dict[str, list[_SourceState]] = {}
        for state in states:
            self._validate(state)
            for part in state._sources:
                by_source.setdefault(part.source, []).append(part)
        merged = []
        for source, parts in sorted(by_source.items()):
            specs = self._specs(source)
            # Remove duplicates and contained states before testing overlap.
            # This also handles a diamond whose two paths carry the same input.
            kept: list[_SourceState] = []
            for part in sorted(parts, key=lambda p: len(p.identities), reverse=True):
                if any(
                    part.identities == other.identities and part.values != other.values
                    for other in kept
                ):
                    raise IntermediateError("Contributor snapshot changed between captures")
                if any(part.identities <= other.identities for other in kept):
                    continue
                kept.append(part)
            if any(not s.idempotent for s in specs):
                for n, part in enumerate(kept):
                    if any(part.identities & other.identities for other in kept[n + 1 :]):
                        raise IntermediateError(
                            "Partially overlapping additive intermediate states cannot be merged "
                            "without splitting an input; retain disjoint inputs instead"
                        )
            values = tuple(
                self._merge_value(spec, [p.values[n] for p in kept]) for n, spec in enumerate(specs)
            )
            identities = frozenset().union(*(p.identities for p in kept))
            merged.append(_SourceState(source, identities, values))
        return PolicyIntermediateState(self._owner, tuple(merged))

    def _merge_value(self, spec: Intermediate, values: list[tuple]) -> tuple:
        width = 2 if spec.aggregate in {"unique", "avg"} else 1
        aliases = ", ".join(f"v{n}" for n in range(width))
        rows = ", ".join("(" + ", ".join("?" for _ in range(width)) + ")" for _ in values)
        if spec.aggregate == "unique":
            aggregation = (
                "CASE WHEN MAX(v0) = 2 THEN 2 ELSE LEAST(COUNT(DISTINCT v1), 2) END, MIN(v1)"
            )
        elif spec.aggregate == "avg":
            aggregation = "SUM(v0), SUM(v1)"
        else:
            function = "sum" if spec.aggregate == "count" else spec.aggregate
            value = "CAST(v0 AS BOOLEAN)" if function in {"bool_and", "bool_or"} else "v0"
            aggregation = f"{function}({value})"
        return tuple(
            self.connection.execute(
                f"SELECT {aggregation} FROM (VALUES {rows}) AS states({aliases})",
                [v for row in values for v in row],
            ).fetchone()
        )

    def values(self, state: PolicyIntermediateState) -> dict[str, Any]:
        """Finalize metrics; unique values retain explicit cardinality metadata.

        ``unique`` yields ``(0, None)``, ``(1, value)`` or ``(2, None)``.
        NULLs are ignored as in COUNT(DISTINCT), MIN, and SQL aggregates.
        """
        self._validate(state)
        parts = {p.source: p for p in state._sources}
        result = {}
        for source in sorted(self.sources):
            part = parts.get(source)
            for n, spec in enumerate(self._specs(source)):
                if part is None:
                    value = (
                        (0, None)
                        if spec.aggregate == "unique"
                        else (0 if spec.aggregate == "count" else None)
                    )
                else:
                    raw = part.values[n]
                    if spec.aggregate == "unique":
                        value = (raw[0], raw[1] if raw[0] == 1 else None)
                    elif spec.aggregate == "avg":
                        value = self.connection.execute("SELECT ? / NULLIF(?, 0)", raw).fetchone()[
                            0
                        ]
                    else:
                        value = raw[0]
                result[spec.name] = value
        return result

    def applicable(self, state: PolicyIntermediateState) -> bool:
        """Ordinary SOURCE requires every source to have a contributor.

        REQUIRED sources cause evaluation even when missing, so absence cannot
        silently skip the policy. ``evaluate`` fails absent required sources.
        """
        self._validate(state)
        present = {p.source for p in state._sources if p.identities}
        return bool(self.required_sources - present) or self.sources <= present

    def evaluate(
        self, state: PolicyIntermediateState, predicate: str, *, sink: dict[str, Any] | None = None
    ) -> bool | None:
        """Evaluate a trusted final predicate; None means not applicable.

        Metric names are SQL columns. Unique metrics expose ``name`` (the sole
        value, otherwise NULL) and ``name__cardinality`` (0/1/2). Write an
        explicit cardinality guard where required by the policy. Sink values
        are bound parameters. This returns a verdict; the existing resolution
        layer remains responsible for REMOVE/KILL/RETRY and side effects.
        """
        if not self.applicable(state):
            return None
        present = {p.source for p in state._sources if p.identities}
        if not self.required_sources <= present:
            return False
        parsed = sqlglot.parse_one(predicate, read="duckdb")
        if any(isinstance(n, (exp.Query, exp.AggFunc, exp.Window)) for n in parsed.walk()):
            raise IntermediateError("Final predicate must be a scalar expression")
        fields: dict[str, Any] = {}
        values = self.values(state)
        for spec in self.intermediates:
            value = values[spec.name]
            if spec.aggregate == "unique":
                fields[spec.name + "__cardinality"], fields[spec.name] = value
            else:
                fields[spec.name] = value
        for name, value in (sink or {}).items():
            if name in fields:
                raise IntermediateError(f"Sink column shadows intermediate: {name}")
            fields[name] = value
        projection = ", ".join(f"? AS {_quote(name)}" for name in fields) or "1"
        sql = f"SELECT ({parsed.sql(dialect='duckdb')}) IS TRUE FROM (SELECT {projection})"
        return self.connection.execute(sql, list(fields.values())).fetchone()[0]
