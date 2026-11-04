"""Compile the source-local subset of PGN to intermediate-state demands."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

import sqlglot
from sqlglot import exp

from .intermediates import (
    Intermediate,
    IntermediateError,
    PolicyIntermediates,
    PolicyIntermediateState,
)
from .policy import Policy


@dataclass(frozen=True)
class CompiledIntermediatePolicy:
    plan: PolicyIntermediates
    predicate: str
    _sink_columns: tuple[tuple[str, str], ...]

    def evaluate(self, state: PolicyIntermediateState, *, sink: dict[str, Any] | None = None):
        supplied = sink or {}
        missing = {column for _, column in self._sink_columns} - supplied.keys()
        if missing:
            raise IntermediateError(f"Missing sink columns: {sorted(missing)}")
        return self.plan.evaluate(
            state,
            self.predicate,
            sink={name: supplied[column] for name, column in self._sink_columns},
        )


def compile_policy_intermediates(connection: Any, policy: Policy) -> CompiledIntermediatePolicy:
    """Compile aggregates and unique source expressions, retaining sink predicates.

    Reject unsupported policy constructs instead of treating them as fresh source
    values or silently dropping a demand. This does not register a policy or run
    its resolution; the trusted caller owns contributor capture and enforcement.
    """
    if policy.dimensions or policy.dimension_queries:
        raise IntermediateError("Intermediate compilation does not support DIMENSION")
    root = sqlglot.parse_one(policy.constraint, read="duckdb")
    if any(isinstance(n, (exp.Query, exp.Window, exp.Filter)) for n in root.walk()):
        raise IntermediateError("Subqueries, windows and filtered aggregates are unsupported")
    sources = {s.lower(): s for s in policy.sources}
    aliases = dict(sources)
    aliases.update({a.lower(): b for a, b in (policy.source_aliases or {}).items()})
    sink_names = {"_output_"}
    if policy.sink:
        sink_names.add(policy.sink.lower())
    if policy.sink_alias:
        sink_names.add(policy.sink_alias.lower())
    specs: list[Intermediate] = []
    sink_columns: dict[str, str] = {}

    def source_of(column: exp.Column) -> str | None:
        qualifier = ".".join(p.name for p in column.parts[:-1]).lower()
        if qualifier in aliases:
            return aliases[qualifier]
        if qualifier in sink_names:
            return None
        raise IntermediateError(f"Unbound intermediate policy column: {column.sql()}")

    def demand(expression: exp.Expression, aggregate: str) -> exp.Column:
        columns = list(expression.find_all(exp.Column))
        involved = {source_of(c) for c in columns}
        if not columns and len(sources) == 1:
            involved = {next(iter(sources.values()))}
        if len(involved) != 1 or None in involved:
            raise IntermediateError("An intermediate expression must belong to exactly one SOURCE")
        source = involved.pop()
        local = expression.transform(
            lambda n: exp.column(n.name, quoted=True) if isinstance(n, exp.Column) else n
        )
        name = f"_metric_{len(specs)}"
        specs.append(Intermediate(name, source, local.sql(dialect="duckdb"), aggregate))
        return exp.column(name)

    comparisons = (
        exp.EQ,
        exp.NEQ,
        exp.GT,
        exp.GTE,
        exp.LT,
        exp.LTE,
        exp.Like,
        exp.ILike,
        exp.Between,
        exp.In,
    )
    booleans = (exp.And, exp.Or, exp.Not)
    aggregate_names = {
        "MIN": "min",
        "MAX": "max",
        "SUM": "sum",
        "COUNT": "count",
        "AVG": "avg",
        "LOGICAL_AND": "bool_and",
        "LOGICAL_OR": "bool_or",
    }

    def visit(node: exp.Expression) -> tuple[exp.Expression, list[exp.Expression]]:
        if isinstance(node, exp.Not):
            for column in node.find_all(exp.Column):
                ancestor = column.parent
                within_aggregate = False
                while ancestor is not node and ancestor is not None:
                    within_aggregate |= isinstance(ancestor, exp.AggFunc)
                    ancestor = ancestor.parent
                if source_of(column) is not None and not within_aggregate:
                    raise IntermediateError(
                        "Negated implicit source uniqueness is unsupported; use explicit aggregates"
                    )
        if isinstance(node, exp.AggFunc):
            kind = aggregate_names.get(node.sql_name())
            if kind is None or node.expressions:
                raise IntermediateError(f"Unsupported policy aggregate: {node.sql()}")
            argument = node.this
            if isinstance(argument, exp.Distinct):
                # The PGN parser expands UNIQUE/NOT UNIQUE to this comparison.
                parent = node.parent
                if (
                    kind != "count"
                    or len(argument.expressions) != 1
                    or not isinstance(parent, (exp.EQ, exp.NEQ))
                    or parent.this is not node
                    or not isinstance(parent.expression, exp.Literal)
                    or parent.expression.this != "1"
                    or parent.expression.is_string
                ):
                    raise IntermediateError("Only COUNT(DISTINCT expression) =/!= 1 is compact")
                metric = demand(argument.expressions[0], "unique")
                return exp.column(metric.name + "__cardinality"), []
            return demand(argument, kind), []
        # Anonymous calls may be aggregates unknown to sqlglot. Do not let one
        # become a purported contributor-local scalar (notably ARBITRARY).
        if isinstance(node, exp.Anonymous):
            raise IntermediateError(f"Unclassified policy function: {node.name}")
        columns = list(node.find_all(exp.Column))
        if (
            columns
            and not isinstance(node, (*comparisons, *booleans, exp.Paren, exp.Is))
            and not any(isinstance(n, (exp.AggFunc, exp.Anonymous)) for n in node.walk())
            and all(source_of(c) is not None for c in columns)
        ):
            # A joint expression is evaluated per contributor before uniqueness.
            metric = demand(node, "unique")
            guard = exp.EQ(
                this=exp.column(metric.name + "__cardinality"), expression=exp.Literal.number(1)
            )
            return metric, [guard]
        if isinstance(node, exp.Column):
            source_of(node)  # validates binding
            name = f"_sink_{len(sink_columns)}"
            sink_columns[name] = node.name
            return exp.column(name), []
        copied = node.copy()
        guards: list[exp.Expression] = []
        for key, value in node.args.items():
            if isinstance(value, exp.Expression):
                child, pending = visit(value)
                if isinstance(node, booleans):
                    child = guarded(child, pending)
                else:
                    guards.extend(pending)
                copied.set(key, child)
            elif isinstance(value, list):
                children = []
                for child in value:
                    if isinstance(child, exp.Expression):
                        child, pending = visit(child)
                        guards.extend(pending)
                    children.append(child)
                copied.set(key, children)
        if isinstance(node, (*comparisons, exp.Is)):
            return guarded(copied, guards), []
        return copied, guards

    def guarded(node: exp.Expression, guards: list[exp.Expression]) -> exp.Expression:
        for guard in guards:
            node = exp.and_(guard, node)
        return node

    predicate, guards = visit(root)
    return CompiledIntermediatePolicy(
        PolicyIntermediates(
            connection,
            specs,
            sources=policy.sources,
            required_sources=policy.required_sources or (),
        ),
        guarded(predicate, guards).sql(dialect="duckdb"),
        tuple(sink_columns.items()),
    )
