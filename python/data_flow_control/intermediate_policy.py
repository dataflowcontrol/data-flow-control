"""Adapters for the Rust compiled-policy intermediate demand interface."""

from __future__ import annotations

from dataclasses import dataclass
import json
from typing import Any

from .intermediates import Intermediate, PolicyIntermediates, PolicyIntermediateState
from .planner import Planner, _policy_specs_json
from .policy import Policy


@dataclass(frozen=True)
class CompiledIntermediatePolicy:
    plan: PolicyIntermediates
    predicate: str
    _sink_columns: tuple[tuple[str, str], ...]

    def evaluate(self, state: PolicyIntermediateState, *, sink: dict[str, Any] | None = None):
        return self.plan._native.evaluate_bound(
            self.plan.connection, state, self.predicate, list(self._sink_columns), sink or {}
        )


def _from_registered(connection, planner, index, policy):
    compilation = json.loads(planner.inner.compile_intermediate_policy(index))
    return _from_compilation(connection, compilation, policy)


def _from_compilation(connection, compilation, policy):
    return CompiledIntermediatePolicy(
        PolicyIntermediates(
            connection,
            [Intermediate(**spec) for spec in compilation["specs"]],
            sources=compilation.get("sources", policy.sources),
            required_sources=compilation.get("required_sources", policy.required_sources or ()),
        ),
        compilation["predicate"],
        tuple(tuple(pair) for pair in compilation["sink_columns"]),
    )


def compile_policy_intermediates(connection: Any, policy: Policy) -> CompiledIntermediatePolicy:
    """Use the ordinary Rust compiler for a trusted standalone plan definition."""
    planner = Planner()
    compilation = json.loads(
        planner.inner.compile_intermediate_definition(_policy_specs_json([policy]))
    )
    return _from_compilation(connection, compilation, policy)
