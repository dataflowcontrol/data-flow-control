"""Python API adapters for Rust-owned policy intermediate plans and states."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Any, Iterable
import json

from ._passant import (
    IntermediateError as IntermediateError,
    PyIntermediatePlan,
    PyIntermediateState as PolicyIntermediateState,
    validate_intermediate_spec_json,
    validate_intermediate_transfer_type,
)


def _quote(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


def validate_transfer_type(sql_type: str, *, identity: bool = False) -> None:
    validate_intermediate_transfer_type(str(sql_type), identity)


@dataclass(frozen=True)
class Intermediate:
    """A source-local demand; validation and aggregate semantics belong to Rust."""

    name: str
    source: str
    expression: str
    aggregate: str = "unique"

    def __post_init__(self) -> None:
        validate_intermediate_spec_json(json.dumps(asdict(self)))


class PolicyIntermediates:
    """Database execution adapter for an opaque native intermediate plan."""

    def __init__(
        self,
        connection: Any,
        intermediates: Iterable[Intermediate],
        *,
        sources: Iterable[str] | None = None,
        required_sources: Iterable[str] = (),
        _sink_endpoint: bool = True,
    ) -> None:
        self.connection = connection
        self.intermediates = tuple(intermediates)
        self.sources = frozenset(
            sources if sources is not None else (i.source for i in self.intermediates)
        )
        self.required_sources = frozenset(required_sources)
        self._native = PyIntermediatePlan(
            json.dumps([asdict(i) for i in self.intermediates]),
            sorted(self.sources),
            sorted(self.required_sources),
            _sink_endpoint,
        )

    def empty(self) -> PolicyIntermediateState:
        return self._native.empty()

    def capture(self, source: str, query: str, *, identity_columns: Iterable[str]):
        return self._native.capture(self.connection, source, query, list(identity_columns))

    def merge(self, *states: PolicyIntermediateState) -> PolicyIntermediateState:
        return self._native.merge(self.connection, list(states))

    def values(self, state: PolicyIntermediateState) -> dict[str, Any]:
        return self._native.values(self.connection, state)

    def applicable(self, state: PolicyIntermediateState) -> bool:
        return self._native.applicable(state)

    def evaluate(self, state, predicate: str, *, sink: dict[str, Any] | None = None):
        return self._native.evaluate(self.connection, state, predicate, sink or {})
