from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from ._rust import (
    normalize_policy_dimension_aliases,
    normalize_policy_dimension_queries,
    normalize_policy_dimensions,
    normalize_policy_source_aliases,
    normalize_policy_sources,
    parse_policy_to_json,
    resolution_to_python,
    validate_constraint_expression,
)


class Resolution(Enum):
    REMOVE = "REMOVE"
    KILL = "KILL"
    UDF = "UDF"
    RELATION_UDF = "RELATION UDF"

    @classmethod
    def from_label(cls, label: str) -> Resolution:
        upper = label.strip().upper()
        if upper == "REMOVE":
            return cls.REMOVE
        if upper == "KILL":
            return cls.KILL
        if upper.startswith("UDF "):
            return cls.UDF
        if upper.startswith("RELATION UDF "):
            return cls.RELATION_UDF
        return cls(label)


@dataclass(eq=True)
class Policy:
    constraint: str
    on_fail: Resolution
    sources: list[str]
    sink: str | None = None
    sink_alias: str | None = None
    source_aliases: dict[str, str] | None = None
    description: str | None = None
    required_sources: list[str] | None = None
    dimensions: list[str] | None = None
    dimension_aliases: dict[str, str] | None = None
    dimension_queries: dict[str, str] | None = None
    udf_name: str | None = None

    def __post_init__(self) -> None:
        if self.sources is None:
            raise ValueError("Sources must be provided (use an empty list for no sources)")
        if not isinstance(self.sources, list):
            raise ValueError("Sources must be provided as a list of table names")
        raw_sources = list(self.sources)
        if self.source_aliases is None:
            self.source_aliases = normalize_policy_source_aliases(raw_sources)
        else:
            self.source_aliases = dict(self.source_aliases)
        self.sources = normalize_policy_sources(raw_sources)
        self.required_sources = _normalize_optional_sources(self.required_sources)
        raw_dimensions = list(self.dimensions or [])
        if self.dimension_aliases is None:
            self.dimension_aliases = normalize_policy_dimension_aliases(raw_dimensions)
        else:
            self.dimension_aliases = dict(self.dimension_aliases)
        if self.dimension_queries is None:
            self.dimension_queries = normalize_policy_dimension_queries(raw_dimensions)
        else:
            self.dimension_queries = dict(self.dimension_queries)
        self.dimensions = normalize_policy_dimensions(raw_dimensions)
        source_keys = {source.lower() for source in self.sources}
        missing_required = [
            source for source in self.required_sources if source.lower() not in source_keys
        ]
        if missing_required:
            missing = ", ".join(sorted(missing_required))
            raise ValueError(f"Required sources must also be listed in sources: {missing}")
        if not isinstance(self.on_fail, Resolution):
            self.on_fail = Resolution.from_label(str(self.on_fail))
        if self.on_fail in (Resolution.UDF, Resolution.RELATION_UDF) and not self.udf_name:
            raise ValueError("udf_name is required for UDF resolutions")
        if not self.sources and self.sink is None:
            raise ValueError("Either sources or sink must be provided")
        if self.sink_alias is not None and self.sink is None:
            raise ValueError("sink_alias requires sink to be provided")
        validate_constraint_expression(self.constraint, "constraint")

    @property
    def on_fail_label(self) -> str:
        if self.on_fail == Resolution.UDF and self.udf_name:
            return f"UDF {self.udf_name}"
        if self.on_fail == Resolution.RELATION_UDF and self.udf_name:
            return f"RELATION UDF {self.udf_name}"
        return self.on_fail.value

    @classmethod
    def from_pgn(cls, policy_str: str) -> Policy:
        parsed = parse_policy_to_json(policy_str)
        if "Pgn" not in parsed:
            raise ValueError("Policy text did not parse as a PGN policy")
        spec = parsed["Pgn"]
        on_fail_label = resolution_to_python(spec["on_fail"])
        on_fail = Resolution.from_label(on_fail_label)
        udf_name = None
        if on_fail == Resolution.UDF:
            udf_name = on_fail_label[4:].strip()
        elif on_fail == Resolution.RELATION_UDF:
            udf_name = on_fail_label[13:].strip()
        dimension_entries = _dimension_entries_from_spec(spec)
        return cls(
            constraint=spec["constraint"],
            on_fail=on_fail,
            sources=spec["sources"],
            required_sources=spec.get("required_sources", []),
            sink=spec.get("sink"),
            sink_alias=spec.get("sink_alias"),
            source_aliases=spec.get("source_aliases", {}),
            description=spec.get("description"),
            dimensions=dimension_entries,
            dimension_aliases=spec.get("dimension_aliases", {}),
            dimension_queries=spec.get("dimension_queries", {}),
            udf_name=udf_name,
        )


def _dimension_entries_from_spec(spec: dict) -> list[str]:
    tables = spec.get("dimension_tables") or spec.get("dimensions") or []
    aliases = spec.get("dimension_aliases") or {}
    queries = spec.get("dimension_queries") or {}
    entries: list[str] = []
    for table in tables:
        alias = next((alias for alias, base in aliases.items() if base == table), None)
        if alias is not None:
            entries.append(f"{table} {alias}")
        else:
            entries.append(table)
    for alias, query in queries.items():
        entries.append(f"{query} {alias}")
    return entries


def _normalize_optional_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        return []
    return normalize_policy_sources(sources)
