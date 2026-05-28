from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from ._rust import (
    normalize_policy_dimensions,
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
    description: str | None = None
    required_sources: list[str] | None = None
    dimensions: list[str] | None = None

    def __post_init__(self) -> None:
        self.sources = normalize_policy_sources(self.sources)
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
    def from_policy_str(cls, policy_str: str) -> Policy:
        parsed = parse_policy_to_json(policy_str)
        if "Dfc" not in parsed:
            raise ValueError("Policy text did not parse as a Policy")
        spec = parsed["Dfc"]
        return cls(
            constraint=spec["constraint"],
            on_fail=Resolution(resolution_to_python(spec["on_fail"])),
            sources=spec["sources"],
            required_sources=spec.get("required_sources", []),
            sink=spec.get("sink"),
            sink_alias=spec.get("sink_alias"),
            description=spec.get("description"),
            dimensions=spec.get("dimensions", []),
        )


@dataclass(eq=True)
class PgnPolicy:
    text: str

    @classmethod
    def from_text(cls, text: str) -> PgnPolicy:
        return cls(text=text)


def _normalize_optional_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        return []
    return normalize_policy_sources(sources)


def _normalize_optional_dimensions(dimensions: list[str] | None) -> list[str]:
    if dimensions is None:
        return []
    return normalize_policy_dimensions(dimensions)
