from __future__ import annotations

from enum import Enum
import re

import sqlglot
from sqlglot import exp

from .sqlglot_utils import get_column_name, get_table_name_from_column


class Resolution(Enum):
    REMOVE = "REMOVE"
    KILL = "KILL"
    INVALIDATE = "INVALIDATE"
    INVALIDATE_MESSAGE = "INVALIDATE_MESSAGE"
    LLM = "LLM"


def _normalize_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        raise ValueError("Sources must be provided (use an empty list for no sources)")
    if not isinstance(sources, list):
        raise ValueError("Sources must be provided as a list of table names")
    if any(source is None for source in sources):
        raise ValueError("Sources cannot contain None values")

    seen: set[str] = set()
    normalized: list[str] = []
    for source in sources:
        if not isinstance(source, str) or not source.strip():
            raise ValueError("Sources must be non-empty strings")
        stripped = source.strip()
        key = stripped.lower()
        if key in seen:
            raise ValueError(f"Duplicate source table '{stripped}' in sources list")
        seen.add(key)
        normalized.append(stripped)
    return normalized


def _validate_table_name(table_name: str, table_type: str) -> None:
    try:
        parsed = sqlglot.parse_one(f"SELECT * FROM {table_name}", read="duckdb")
    except sqlglot.errors.ParseError as exc:
        raise ValueError(f"Invalid {table_type.lower()} table '{table_name}': {exc}") from exc

    if not isinstance(parsed, exp.Select) or not list(parsed.find_all(exp.Table)):
        raise ValueError(f"Invalid {table_type.lower()} table '{table_name}'")


def _validate_identifier_name(identifier: str, identifier_type: str) -> None:
    try:
        sqlglot.parse_one(f"SELECT 1 FROM tbl AS {identifier}", read="duckdb")
    except sqlglot.errors.ParseError as exc:
        raise ValueError(f"Invalid {identifier_type.lower()} '{identifier}': {exc}") from exc


def _parse_constraint_expression(constraint: str) -> exp.Expression:
    try:
        parsed = sqlglot.parse_one(f"SELECT {constraint} AS test", read="duckdb")
    except sqlglot.errors.ParseError as exc:
        if constraint.strip().upper().startswith("SELECT"):
            raise ValueError("Constraint must be an expression, not a SELECT statement") from exc
        raise ValueError(f"Invalid constraint SQL expression '{constraint}': {exc}") from exc

    if not isinstance(parsed, exp.Select) or not parsed.expressions:
        raise ValueError(f"Invalid constraint SQL expression '{constraint}'")
    expr = parsed.expressions[0]
    expr = expr.this if isinstance(expr, exp.Alias) else expr
    if isinstance(expr, exp.Select):
        raise ValueError("Constraint must be an expression, not a SELECT statement")
    return expr


def _extract_keyword_values(policy_str: str, *, require_aggregate: bool) -> dict[str, str]:
    normalized = re.sub(r"\s+", " ", policy_str.strip())
    if not normalized:
        raise ValueError("Policy text is empty")

    if require_aggregate:
        if not normalized.upper().startswith("AGGREGATE "):
            raise ValueError("Aggregate policy text requires 'AGGREGATE' keyword")
        normalized = normalized[len("AGGREGATE ") :]

    values: dict[str, str] = {}
    positions: list[tuple[int, str]] = []
    for keyword in ["SOURCES", "SINK", "CONSTRAINT", "DESCRIPTION"]:
        for match in re.finditer(rf"\b{keyword}\b", normalized, re.IGNORECASE):
            positions.append((match.start(), keyword))
    for match in re.finditer(r"\bON\s+FAIL\b", normalized, re.IGNORECASE):
        positions.append((match.start(), "ON FAIL"))
    positions.sort()

    for idx, (pos, keyword) in enumerate(positions):
        value_start = pos + (7 if keyword == "ON FAIL" else len(keyword))
        while value_start < len(normalized) and normalized[value_start] == " ":
            value_start += 1
        value_end = positions[idx + 1][0] if idx + 1 < len(positions) else len(normalized)
        while value_end > value_start and normalized[value_end - 1] == " ":
            value_end -= 1
        values[keyword] = normalized[value_start:value_end]
    return values


class DFCPolicy:
    def __init__(
        self,
        constraint: str,
        on_fail: Resolution,
        sources: list[str],
        sink: str | None = None,
        sink_alias: str | None = None,
        description: str | None = None,
    ) -> None:
        self.sources = _normalize_sources(sources)
        if not self.sources and sink is None:
            raise ValueError("Either sources or sink must be provided")
        if sink_alias is not None and sink is None:
            raise ValueError("sink_alias requires sink to be provided")

        self.sink = sink
        self.sink_alias = sink_alias.strip() if isinstance(sink_alias, str) else sink_alias
        self.constraint = constraint
        self.on_fail = on_fail
        self.description = description
        self._sources_lower = {source.lower() for source in self.sources}
        self._sink_reference_names: set[str] = set()
        if self.sink:
            sink_lower = self.sink.lower()
            if sink_lower not in self._sources_lower or not self.sink_alias:
                self._sink_reference_names.add(sink_lower)
        if self.sink_alias:
            if not self.sink_alias:
                raise ValueError("sink_alias must be a non-empty string")
            self._sink_reference_names.add(self.sink_alias.lower())

        self._constraint_parsed = _parse_constraint_expression(constraint)
        self._validate()
        self._source_columns_needed = self._calculate_source_columns_needed()

    @classmethod
    def from_policy_str(cls, policy_str: str) -> "DFCPolicy":
        values = _extract_keyword_values(policy_str, require_aggregate=False)
        if "CONSTRAINT" not in values:
            raise ValueError("CONSTRAINT is required but not found in policy text")
        if "ON FAIL" not in values:
            raise ValueError("ON FAIL is required but not found in policy text")

        sources_raw = values.get("SOURCES", "")
        sources = [] if not sources_raw or sources_raw.upper() == "NONE" else [
            item.strip() for item in sources_raw.split(",") if item.strip()
        ]
        sink = values.get("SINK")
        if sink and sink.upper() == "NONE":
            sink = None

        return cls(
            constraint=values["CONSTRAINT"],
            on_fail=Resolution(values["ON FAIL"].upper()),
            sources=sources,
            sink=sink,
            description=values.get("DESCRIPTION") or None,
        )

    def _validate(self) -> None:
        for source in self.sources:
            _validate_table_name(source, "Source")
        if self.sink:
            _validate_table_name(self.sink, "Sink")
        if self.sink_alias:
            _validate_identifier_name(self.sink_alias, "Sink alias")

        self._validate_column_qualification()
        self._validate_aggregation_rules()

    def _validate_column_qualification(self) -> None:
        unqualified_columns = [
            get_column_name(column)
            for column in self._constraint_parsed.find_all(exp.Column)
            if get_table_name_from_column(column) is None
        ]
        if unqualified_columns:
            raise ValueError(
                "All columns in constraints must be qualified with table names. "
                f"Unqualified columns found: {', '.join(unqualified_columns)}"
            )

    def _validate_aggregation_rules(self) -> None:
        aggregate_funcs = list(self._constraint_parsed.find_all(exp.AggFunc))
        all_columns = list(self._constraint_parsed.find_all(exp.Column))

        if aggregate_funcs and not self.sources:
            raise ValueError(
                "Aggregations in constraints can only reference the source tables, "
                "but no sources are provided"
            )

        for agg_func in aggregate_funcs:
            for column in agg_func.find_all(exp.Column):
                table_name = get_table_name_from_column(column)
                if table_name is None:
                    continue
                if table_name in self._sink_reference_names:
                    raise ValueError(
                        f"Aggregation '{agg_func.sql()}' references sink table '{table_name}', "
                        "but aggregations can only reference source tables"
                    )
                if table_name not in self._sources_lower:
                    raise ValueError(
                        f"Aggregation '{agg_func.sql()}' references table '{table_name}', "
                        f"but aggregations can only reference source tables {self.sources}"
                    )

        if self.sources:
            unaggregated_source_columns = []
            for column in all_columns:
                table_name = get_table_name_from_column(column)
                if table_name in self._sources_lower and column.find_ancestor(exp.AggFunc) is None:
                    unaggregated_source_columns.append(f"{table_name}.{get_column_name(column)}")
            if unaggregated_source_columns:
                raise ValueError(
                    "All columns from source tables must be aggregated. "
                    f"Unaggregated source columns found: {', '.join(unaggregated_source_columns)}"
                )

    def _calculate_source_columns_needed(self) -> dict[str, set[str]]:
        needed = {source.lower(): set() for source in self.sources}
        for agg in self._constraint_parsed.find_all(exp.AggFunc):
            for column in agg.find_all(exp.Column):
                table_name = get_table_name_from_column(column)
                if table_name in needed:
                    needed[table_name].add(get_column_name(column).lower())
        for column in self._constraint_parsed.find_all(exp.Column):
            if column.find_ancestor(exp.AggFunc) is not None:
                continue
            table_name = get_table_name_from_column(column)
            if table_name in needed:
                needed[table_name].add(get_column_name(column).lower())
        return needed

    def get_identifier(self) -> str:
        parts: list[str] = []
        if self.sources:
            parts.append(f"sources={self.sources}")
        if self.sink:
            parts.append(f"sink={self.sink}")
        if self.sink_alias:
            parts.append(f"sink_alias={self.sink_alias}")
        parts.append(f"constraint={self.constraint}")
        return f"DFCPolicy({', '.join(parts)})"

    def __repr__(self) -> str:
        parts: list[str] = []
        if self.sources:
            parts.append(f"sources={self.sources!r}")
        if self.sink:
            parts.append(f"sink={self.sink!r}")
        if self.sink_alias:
            parts.append(f"sink_alias={self.sink_alias!r}")
        parts.append(f"constraint={self.constraint!r}")
        parts.append(f"on_fail={self.on_fail.value}")
        if self.description:
            parts.append(f"description={self.description!r}")
        return f"DFCPolicy({', '.join(parts)})"

    def __eq__(self, other: object) -> bool:
        return isinstance(other, DFCPolicy) and (
            self.sources == other.sources
            and self.sink == other.sink
            and self.sink_alias == other.sink_alias
            and self.constraint == other.constraint
            and self.on_fail == other.on_fail
            and self.description == other.description
        )


class AggregateDFCPolicy:
    def __init__(
        self,
        constraint: str,
        on_fail: Resolution,
        sources: list[str],
        sink: str | None = None,
        description: str | None = None,
    ) -> None:
        self.sources = _normalize_sources(sources)
        if not self.sources and sink is None:
            raise ValueError("Either sources or sink must be provided")
        if on_fail != Resolution.INVALIDATE:
            raise ValueError(
                "AggregateDFCPolicy currently only supports INVALIDATE resolution, "
                f"but got {on_fail.value}"
            )

        self.sink = sink
        self.constraint = constraint
        self.on_fail = on_fail
        self.description = description
        self._sources_lower = {source.lower() for source in self.sources}
        self._sink_lower = sink.lower() if sink else None

        self._constraint_parsed = _parse_constraint_expression(constraint)
        self._validate()
        self._source_columns_needed = self._calculate_source_columns_needed()

    @classmethod
    def from_policy_str(cls, policy_str: str) -> "AggregateDFCPolicy":
        values = _extract_keyword_values(policy_str, require_aggregate=True)
        if "CONSTRAINT" not in values:
            raise ValueError("CONSTRAINT is required but not found in policy text")
        if "ON FAIL" not in values:
            raise ValueError("ON FAIL is required but not found in policy text")

        sources_raw = values.get("SOURCES", "")
        sources = [] if not sources_raw or sources_raw.upper() == "NONE" else [
            item.strip() for item in sources_raw.split(",") if item.strip()
        ]
        sink = values.get("SINK")
        if sink and sink.upper() == "NONE":
            sink = None
        return cls(
            constraint=values["CONSTRAINT"],
            on_fail=Resolution(values["ON FAIL"].upper()),
            sources=sources,
            sink=sink,
            description=values.get("DESCRIPTION") or None,
        )

    def _validate(self) -> None:
        for source in self.sources:
            _validate_table_name(source, "Source")
        if self.sink:
            _validate_table_name(self.sink, "Sink")
        self._validate_column_qualification()
        self._validate_aggregation_rules()

    def _validate_column_qualification(self) -> None:
        unqualified = []
        for column in self._constraint_parsed.find_all(exp.Column):
            table_name = get_table_name_from_column(column)
            if table_name is not None:
                continue
            if column.find_ancestor(exp.Filter) is not None:
                continue
            parent = column.parent
            if (
                isinstance(parent, exp.AggFunc)
                and parent.this == column
                and self._sink_lower
                and get_column_name(column).lower() == self._sink_lower
            ):
                continue
            unqualified.append(get_column_name(column))
        if unqualified:
            raise ValueError(
                "All columns in constraints must be qualified with table names. "
                f"Unqualified columns found: {', '.join(unqualified)}"
            )

    def _validate_aggregation_rules(self) -> None:
        all_columns = list(self._constraint_parsed.find_all(exp.Column))
        unaggregated_source_columns = []
        for column in all_columns:
            table_name = get_table_name_from_column(column)
            if table_name in self._sources_lower and column.find_ancestor(exp.AggFunc) is None:
                unaggregated_source_columns.append(f"{table_name}.{get_column_name(column)}")
        if unaggregated_source_columns:
            raise ValueError(
                "All columns from source tables must be aggregated. "
                f"Unaggregated source columns found: {', '.join(unaggregated_source_columns)}"
            )

    def _calculate_source_columns_needed(self) -> dict[str, set[str]]:
        needed = {source.lower(): set() for source in self.sources}
        for agg in self._constraint_parsed.find_all(exp.AggFunc):
            for column in agg.find_all(exp.Column):
                table_name = get_table_name_from_column(column)
                if table_name in needed:
                    needed[table_name].add(get_column_name(column).lower())
        return needed

    def get_identifier(self) -> str:
        parts: list[str] = []
        if self.sources:
            parts.append(f"sources={self.sources}")
        if self.sink:
            parts.append(f"sink={self.sink}")
        parts.append(f"constraint={self.constraint}")
        return f"AggregateDFCPolicy({', '.join(parts)})"

    def __repr__(self) -> str:
        parts: list[str] = []
        if self.sources:
            parts.append(f"sources={self.sources!r}")
        if self.sink:
            parts.append(f"sink={self.sink!r}")
        parts.append(f"constraint={self.constraint!r}")
        parts.append(f"on_fail={self.on_fail.value}")
        if self.description:
            parts.append(f"description={self.description!r}")
        return f"AggregateDFCPolicy({', '.join(parts)})"

    def __eq__(self, other: object) -> bool:
        return isinstance(other, AggregateDFCPolicy) and (
            self.sources == other.sources
            and self.sink == other.sink
            and self.constraint == other.constraint
            and self.on_fail == other.on_fail
            and self.description == other.description
        )


__all__ = ["AggregateDFCPolicy", "DFCPolicy", "Resolution"]
