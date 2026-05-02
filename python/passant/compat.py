from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
import json

import duckdb

try:
    from . import _passant
except ImportError:  # pragma: no cover - used before extension is built
    _passant = None


class Resolution(Enum):
    REMOVE = "REMOVE"
    KILL = "KILL"
    INVALIDATE = "INVALIDATE"
    INVALIDATE_MESSAGE = "INVALIDATE_MESSAGE"
    LLM = "LLM"


@dataclass(eq=True)
class DFCPolicy:
    constraint: str
    on_fail: Resolution
    sources: list[str]
    sink: str | None = None
    sink_alias: str | None = None
    description: str | None = None

    @classmethod
    def from_policy_str(cls, policy_str: str) -> "DFCPolicy":
        tokens = policy_str.split()
        normalized = [token.upper() for token in tokens]
        sources: list[str] = []
        sink: str | None = None
        description: str | None = None
        constraint = ""
        on_fail = Resolution.REMOVE

        if "SOURCES" in normalized:
            idx = normalized.index("SOURCES") + 1
            while idx < len(tokens) and normalized[idx] not in {"SINK", "CONSTRAINT", "ON", "DESCRIPTION"}:
                sources.extend(part.strip() for part in tokens[idx].split(",") if part.strip())
                idx += 1
        if "SINK" in normalized:
            idx = normalized.index("SINK") + 1
            sink = tokens[idx]
        if "CONSTRAINT" in normalized:
            start = normalized.index("CONSTRAINT") + 1
            end = normalized.index("ON") if "ON" in normalized else len(tokens)
            constraint = " ".join(tokens[start:end])
        if "FAIL" in normalized:
            idx = normalized.index("FAIL") + 1
            on_fail = Resolution(tokens[idx].upper())
        if "DESCRIPTION" in normalized:
            idx = normalized.index("DESCRIPTION") + 1
            description = " ".join(tokens[idx:])

        return cls(
            constraint=constraint,
            on_fail=on_fail,
            sources=sources,
            sink=sink,
            description=description,
        )


@dataclass(eq=True)
class AggregateDFCPolicy:
    constraint: str
    on_fail: Resolution
    sources: list[str]
    sink: str | None = None
    description: str | None = None


@dataclass(eq=True)
class FlowGuardPolicy:
    text: str

    @classmethod
    def from_text(cls, text: str) -> "FlowGuardPolicy":
        return cls(text=text)


class SQLRewriter:
    def __init__(self, conn=None, stream_file_path=None, bedrock_client=None, bedrock_model_id=None, recorder=None):
        self.conn = conn or duckdb.connect()
        self.stream_file_path = stream_file_path
        self.bedrock_client = bedrock_client
        self.bedrock_model_id = bedrock_model_id
        self.recorder = recorder
        self._policies: list[DFCPolicy | AggregateDFCPolicy | FlowGuardPolicy] = []
        self._planner = _passant.PyPlanner() if _passant is not None else None

    def register_policy(self, policy: DFCPolicy | AggregateDFCPolicy | FlowGuardPolicy) -> None:
        self._policies.append(policy)

    def get_dfc_policies(self) -> list[DFCPolicy]:
        return [policy for policy in self._policies if isinstance(policy, DFCPolicy)]

    def get_aggregate_policies(self) -> list[AggregateDFCPolicy]:
        return [policy for policy in self._policies if isinstance(policy, AggregateDFCPolicy)]

    def delete_policy(self, sources=None, sink=None, constraint="", on_fail=None, description=None) -> bool:
        for idx, policy in enumerate(self._policies):
            if sources is not None and getattr(policy, "sources", None) != sources:
                continue
            if sink is not None and getattr(policy, "sink", None) != sink:
                continue
            if constraint and getattr(policy, "constraint", None) != constraint:
                continue
            if on_fail is not None and getattr(policy, "on_fail", None) != on_fail:
                continue
            if description is not None and getattr(policy, "description", None) != description:
                continue
            del self._policies[idx]
            return True
        return False

    def transform_query(self, query: str, use_two_phase: bool = False) -> str:
        _ = use_two_phase
        if self._planner is None:
            return query
        dfc_policies = self.get_dfc_policies()
        if not dfc_policies:
            return self._planner.transform_query(query)
        first = dfc_policies[0]
        plan_json = self._planner.plan_with_policy(
            query,
            first.sources,
            first.constraint,
            first.sink,
        )
        plan = json.loads(plan_json)
        return plan["chosen"]["rewritten_sql"]

    def explain_rewrite(self, query: str) -> str:
        if self._planner is None:
            return json.dumps({"chosen": {"rewritten_sql": query}}, indent=2)
        return self._planner.explain_rewrite(query)

    def execute(self, query: str, use_two_phase: bool = False):
        rewritten = self.transform_query(query, use_two_phase=use_two_phase)
        executable = _strip_passant_comment(rewritten)
        return self.conn.execute(executable)

    def fetchall(self, query: str, use_two_phase: bool = False):
        return self.execute(query, use_two_phase=use_two_phase).fetchall()

    def fetchone(self, query: str, use_two_phase: bool = False):
        return self.execute(query, use_two_phase=use_two_phase).fetchone()

    def finalize_aggregate_policies(self, sink_table: str) -> dict[str, str | None]:
        return {
            f"aggregate::{policy.constraint}": None
            for policy in self.get_aggregate_policies()
            if policy.sink is None or policy.sink == sink_table
        }

    def get_stream_file_path(self):
        return self.stream_file_path

    def reset_stream_file_path(self) -> None:
        self.stream_file_path = None

    def close(self) -> None:
        self.conn.close()

    def __enter__(self) -> "SQLRewriter":
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        self.close()


def _strip_passant_comment(sql: str) -> str:
    if sql.startswith("-- passant:"):
        return "\n".join(sql.splitlines()[1:])
    return sql
