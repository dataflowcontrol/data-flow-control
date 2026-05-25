from __future__ import annotations

import json
from typing import TYPE_CHECKING

from ._rust import require_extension, resolution_to_python
from .options import RewriteOptions

if TYPE_CHECKING:
    from .policy import AggregatePolicy, PgnPolicy, Policy

try:
    from . import _passant
except ImportError:  # pragma: no cover
    _passant = None


class Planner:
    """Backend-neutral wrapper around Rust `PyPlanner`."""

    def __init__(self, dialect: str = "duckdb") -> None:
        self.dialect = dialect
        require_extension()
        self._planner = _passant.PyPlanner()

    @property
    def inner(self):
        return self._planner

    def sync_catalog(self, snapshot: dict) -> None:
        self._planner.sync_catalog(json.dumps(snapshot))

    def register_policy(self, policy: Policy | AggregatePolicy | PgnPolicy) -> None:
        from .policy import AggregatePolicy, PgnPolicy, Policy

        if isinstance(policy, PgnPolicy):
            self._planner.register_policy_text(policy.text)
            return
        dfc_policies = [policy] if isinstance(policy, Policy) else []
        aggregate_policies = [policy] if isinstance(policy, AggregatePolicy) else []
        policies_json, aggregate_policies_json = _policy_specs_json(
            dfc_policies, aggregate_policies
        )
        self._planner.register_policy_specs(policies_json, aggregate_policies_json)

    def delete_policy(
        self,
        *,
        sources=None,
        sink=None,
        constraint: str = "",
        on_fail=None,
        description=None,
    ) -> bool:
        on_fail_value = on_fail.value if hasattr(on_fail, "value") else on_fail
        return self._planner.delete_policy(
            sources,
            sink,
            constraint or None,
            on_fail_value,
            description,
        )

    def rewrite(
        self,
        sql: str,
        *,
        use_partial_push: bool = False,
        collect_stats: bool = False,
        dialect: str | None = None,
        options: RewriteOptions | None = None,
    ) -> str:
        opts = options or RewriteOptions(
            use_partial_push=use_partial_push,
            collect_stats=collect_stats,
            dialect=dialect,
        )
        if not self._planner.has_registered_policies():
            return self._planner.transform_query(sql)
        return self._planner.transform_registered(
            sql,
            opts.use_partial_push,
            opts.collect_stats,
            opts.dialect,
        )

    def explain(self, sql: str) -> str:
        if not self._planner.has_registered_policies():
            return self._planner.explain_rewrite(sql)
        return self._planner.explain_rewrite_registered(sql)

    def explain_dict(self, sql: str) -> dict:
        return json.loads(self.explain(sql))

    def last_rewrite_stats(self):
        return self._planner.last_rewrite_stats()

    def last_statement_rewrite_summary(self):
        return self._planner.last_statement_rewrite_summary()

    def has_registered_policies(self) -> bool:
        return self._planner.has_registered_policies()

    def policies(self) -> list[Policy]:
        return _dfc_policies_from_rust(self._planner.dfc_policies_json())

    def aggregate_policies(self) -> list[AggregatePolicy]:
        return _aggregate_policies_from_rust(self._planner.aggregate_policies_json())

    def pgn_policies(self) -> list[PgnPolicy]:
        return _pgn_policies_from_rust(self._planner.pgn_policies_json())


def _policy_specs_json(
    dfc_policies: list[Policy],
    aggregate_policies: list[AggregatePolicy],
) -> tuple[str, str]:
    policies_json = json.dumps(
        [
            {
                "sources": policy.sources,
                "required_sources": policy.required_sources,
                "dimensions": policy.dimensions,
                "sink": policy.sink,
                "sink_alias": policy.sink_alias,
                "constraint": policy.constraint,
                "on_fail": policy.on_fail.value,
                "description": policy.description,
            }
            for policy in dfc_policies
        ]
    )
    aggregate_policies_json = json.dumps(
        [
            {
                "sources": policy.sources,
                "dimensions": policy.dimensions,
                "sink": policy.sink,
                "constraint": policy.constraint,
                "description": policy.description,
            }
            for policy in aggregate_policies
        ]
    )
    return policies_json, aggregate_policies_json


def _dfc_policies_from_rust(policies_json: str) -> list[Policy]:
    from .policy import Policy, Resolution

    policies: list[Policy] = []
    for entry in json.loads(policies_json):
        if "CompatDfc" not in entry:
            continue
        spec = entry["CompatDfc"]
        policies.append(
            Policy(
                constraint=spec["constraint"],
                on_fail=Resolution(resolution_to_python(spec["on_fail"])),
                sources=spec["sources"],
                required_sources=spec.get("required_sources", []),
                sink=spec.get("sink"),
                sink_alias=spec.get("sink_alias"),
                description=spec.get("description"),
                dimensions=spec.get("dimensions", []),
            )
        )
    return policies


def _aggregate_policies_from_rust(policies_json: str) -> list[AggregatePolicy]:
    from .policy import AggregatePolicy, Resolution

    policies: list[AggregatePolicy] = []
    for entry in json.loads(policies_json):
        if "CompatAggregate" not in entry:
            continue
        spec = entry["CompatAggregate"]
        policies.append(
            AggregatePolicy(
                constraint=spec["constraint"],
                on_fail=Resolution.REMOVE,
                sources=spec["sources"],
                sink=spec.get("sink"),
                description=spec.get("description"),
                dimensions=spec.get("dimensions", []),
            )
        )
    return policies


def _pgn_policies_from_rust(policies_json: str) -> list[PgnPolicy]:
    from .policy import PgnPolicy

    policies: list[PgnPolicy] = []
    for entry in json.loads(policies_json):
        if "NativePgn" not in entry:
            continue
        spec = entry["NativePgn"]
        text = spec.get("source_text")
        if not text:
            continue
        policies.append(PgnPolicy(text=text))
    return policies
