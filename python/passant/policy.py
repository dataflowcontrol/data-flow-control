from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from ._rust import (
    normalize_policy_dimensions,
    normalize_policy_source_aliases,
    normalize_policy_sources,
    parse_policy_to_json,
    validate_constraint_expression,
    resolution_to_python,
)


class Resolution(Enum):
    REMOVE = "REMOVE"
    KILL = "KILL"


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
        self.dimensions = _normalize_optional_dimensions(self.dimensions)
        source_keys = {source.lower() for source in self.sources}
        missing_required = [
            source for source in self.required_sources if source.lower() not in source_keys
        ]
        if missing_required:
            missing = ", ".join(sorted(missing_required))
            raise ValueError(f"Required sources must also be listed in sources: {missing}")
        if not isinstance(self.on_fail, Resolution):
            self.on_fail = Resolution(str(self.on_fail).upper())
        if not self.sources and self.sink is None:
            raise ValueError("Either sources or sink must be provided")
        if self.sink_alias is not None and self.sink is None:
            raise ValueError("sink_alias requires sink to be provided")
        validate_constraint_expression(self.constraint, "constraint")
        for dimension in self.dimensions:
            validate_constraint_expression(dimension, "dimension")

    @classmethod
    def from_pgn(cls, policy_str: str) -> Policy:
        parsed = parse_policy_to_json(policy_str)
        if "Pgn" not in parsed:
            raise ValueError("Policy text did not parse as a PGN policy")
        spec = parsed["Pgn"]
        return cls(
            constraint=spec["constraint"],
            on_fail=Resolution(resolution_to_python(spec["on_fail"])),
            sources=spec["sources"],
            required_sources=spec.get("required_sources", []),
            sink=spec.get("sink"),
            sink_alias=spec.get("sink_alias"),
            source_aliases=spec.get("source_aliases", {}),
            description=spec.get("description"),
            dimensions=spec.get("dimensions", []),
        )


def _normalize_optional_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        return []
    return normalize_policy_sources(sources)


def _normalize_optional_dimensions(dimensions: list[str] | None) -> list[str]:
    if dimensions is None:
        return []
    return normalize_policy_dimensions(dimensions)
