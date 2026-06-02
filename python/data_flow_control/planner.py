from __future__ import annotations

import json

from . import _passant
from ._rust import resolution_to_python
from .options import RewriteOptions
from .policy import Policy


class Planner:
    """Backend-neutral wrapper around Rust `PyPlanner`."""

    def __init__(self, dialect: str = "duckdb") -> None:
        self.dialect = dialect
        self._planner = _passant.PyPlanner()

    @property
    def inner(self):
        return self._planner

    def sync_catalog(self, snapshot: dict) -> None:
        self._planner.sync_catalog(json.dumps(snapshot))

    def register_aggregate_function_name(
        self,
        name: str,
        *,
        schema: str | None = None,
        classification: str | None = None,
    ) -> None:
        self._planner.register_aggregate_function_name(name, schema, classification)

    def register_policy(self, policy: Policy) -> None:
        self._planner.register_policy_specs(_policy_specs_json([policy]))

    def register_policies(self, policies: list[Policy]) -> None:
        if not policies:
            return
        self._planner.register_policy_specs(_policy_specs_json(policies))

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

    def rewrite(self, sql: str, *, options: RewriteOptions | None = None) -> str:
        opts = options or RewriteOptions()
        if not self._planner.has_registered_policies():
            return self._planner.transform_query(sql)
        return self._planner.transform_registered(
            sql,
            opts.use_partial_push,
            opts.collect_stats,
            opts.dialect,
            opts.ui_stream_endpoint,
            opts.ui_update_mode.value,
        )

    def last_ui_followup_sql(self) -> str | None:
        return self._planner.last_ui_followup_sql()

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
        return _policies_from_rust(self._planner.policies_json())


def _policy_specs_json(policies: list[Policy]) -> str:
    return json.dumps(
        [
            {
                "sources": policy.sources,
                "required_sources": policy.required_sources,
                "dimensions": policy.dimensions,
                "dimension_aliases": policy.dimension_aliases,
                "dimension_queries": policy.dimension_queries,
                "sink": policy.sink,
                "sink_alias": policy.sink_alias,
                "source_aliases": policy.source_aliases,
                "constraint": policy.constraint,
                "on_fail": policy.on_fail_label,
                "description": policy.description,
            }
            for policy in policies
        ]
    )


def _policies_from_rust(policies_json: str) -> list[Policy]:
    from .policy import Resolution

    policies: list[Policy] = []
    for entry in json.loads(policies_json):
        if "Pgn" not in entry:
            continue
        spec = entry["Pgn"]
        policies.append(
            Policy(
                constraint=spec["constraint"],
                on_fail=Resolution.from_label(resolution_to_python(spec["on_fail"])),
                sources=spec["sources"],
                required_sources=spec.get("required_sources", []),
                sink=spec.get("sink"),
                sink_alias=spec.get("sink_alias"),
                source_aliases=spec.get("source_aliases", {}),
                description=spec.get("description"),
                dimensions=spec.get("dimensions") or spec.get("dimension_tables", []),
                dimension_aliases=spec.get("dimension_aliases", {}),
                dimension_queries=spec.get("dimension_queries", {}),
                udf_name=_udf_name_from_spec(spec),
            )
        )
    return policies


def _udf_name_from_spec(spec: dict) -> str | None:
    label = resolution_to_python(spec["on_fail"])
    if label.startswith("UDF "):
        return label[4:].strip()
    if label.startswith("RELATION UDF "):
        return label[13:].strip()
    return None
