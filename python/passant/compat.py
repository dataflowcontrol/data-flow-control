from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
import json
import tempfile

import duckdb

try:
    from . import _passant
    from ._passant import PassantRewriteError
except ImportError:  # pragma: no cover - used before extension is built
    _passant = None
    PassantRewriteError = None


class Resolution(Enum):
    REMOVE = "REMOVE"
    KILL = "KILL"
    INVALIDATE = "INVALIDATE"
    INVALIDATE_MESSAGE = "INVALIDATE_MESSAGE"
    LLM = "LLM"
    UDF = "UDF"


@dataclass(eq=True)
class DFCPolicy:
    constraint: str
    on_fail: Resolution
    sources: list[str]
    sink: str | None = None
    sink_alias: str | None = None
    description: str | None = None
    required_sources: list[str] | None = None
    dimensions: list[str] | None = None

    def __post_init__(self) -> None:
        self.sources = _normalize_sources(self.sources)
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
        _validate_constraint_expression(self.constraint, "constraint")
        for dimension in self.dimensions:
            _validate_constraint_expression(dimension, "dimension")

    @classmethod
    def from_policy_str(cls, policy_str: str) -> "DFCPolicy":
        parsed = _parse_policy_with_rust(policy_str)
        if "CompatDfc" not in parsed:
            raise ValueError("Policy text did not parse as a DFCPolicy")
        spec = parsed["CompatDfc"]
        return cls(
            constraint=spec["constraint"],
            on_fail=Resolution(_resolution_to_python(spec["on_fail"])),
            sources=spec["sources"],
            required_sources=spec.get("required_sources", []),
            sink=spec.get("sink"),
            sink_alias=spec.get("sink_alias"),
            description=spec.get("description"),
            dimensions=spec.get("dimensions", []),
        )


@dataclass(eq=True)
class AggregateDFCPolicy:
    constraint: str
    on_fail: Resolution
    sources: list[str]
    sink: str | None = None
    description: str | None = None
    dimensions: list[str] | None = None

    def __post_init__(self) -> None:
        self.sources = _normalize_sources(self.sources)
        self.dimensions = _normalize_optional_dimensions(self.dimensions)
        if not isinstance(self.on_fail, Resolution):
            self.on_fail = Resolution(str(self.on_fail).upper())
        if self.on_fail != Resolution.INVALIDATE:
            raise ValueError("AggregateDFCPolicy currently only supports INVALIDATE resolution")
        if not self.sources and self.sink is None:
            raise ValueError("Either sources or sink must be provided")
        _validate_constraint_expression(self.constraint, "constraint")
        for dimension in self.dimensions:
            _validate_constraint_expression(dimension, "dimension")

    @classmethod
    def from_policy_str(cls, policy_str: str) -> "AggregateDFCPolicy":
        parsed = _parse_policy_with_rust(policy_str)
        if "CompatAggregate" not in parsed:
            raise ValueError("Policy text did not parse as an AggregateDFCPolicy")
        spec = parsed["CompatAggregate"]
        return cls(
            constraint=spec["constraint"],
            on_fail=Resolution.INVALIDATE,
            sources=spec["sources"],
            sink=spec.get("sink"),
            description=spec.get("description"),
            dimensions=spec.get("dimensions", []),
        )


@dataclass(eq=True)
class PgnPolicy:
    text: str

    @classmethod
    def from_text(cls, text: str) -> "PgnPolicy":
        return cls(text=text)


class SQLRewriter:
    def __init__(
        self,
        conn=None,
        stream_file_path=None,
        bedrock_client=None,
        bedrock_model_id=None,
        recorder=None,
    ):
        self.conn = conn or duckdb.connect()
        self.stream_file_path = (
            stream_file_path
            or tempfile.NamedTemporaryFile(mode="w", delete=False, suffix=".txt").name
        )
        self.bedrock_client = bedrock_client
        self.bedrock_model_id = bedrock_model_id
        self.recorder = recorder
        self._resolver_functions = {}
        self._planner = _passant.PyPlanner() if _passant is not None else None
        self._register_kill_udf()
        self._register_resolver_udf()

    def _register_kill_udf(self) -> None:
        def _kill() -> bool:
            raise ValueError("KILLing due to dfc policy violation")

        for name in ("kill", "passant_kill"):
            try:
                self.conn.create_function(name, _kill, [], "BOOLEAN")
            except duckdb.Error:
                pass

    def _register_resolver_udf(self) -> None:
        def _address_violating_rows() -> bool:
            return False

        self.register_resolver(_address_violating_rows)

    def register_resolver(self, function, name: str = "address_violating_rows") -> None:
        self._resolver_functions[name] = function
        try:
            self.conn.remove_function(name)
        except duckdb.Error:
            pass
        self.conn.create_function(name, function, [], "BOOLEAN")

    def register_policy(self, policy: DFCPolicy | AggregateDFCPolicy | PgnPolicy) -> None:
        if isinstance(policy, DFCPolicy | AggregateDFCPolicy):
            self._sync_catalog_to_rust()
            self._register_policy_in_rust(policy)
        elif isinstance(policy, PgnPolicy):
            if self._planner is not None:
                self._planner.register_policy_text(policy.text)

    def get_dfc_policies(self) -> list[DFCPolicy]:
        if self._planner is None:
            return []
        return _dfc_policies_from_rust(self._planner.dfc_policies_json())

    def get_aggregate_policies(self) -> list[AggregateDFCPolicy]:
        if self._planner is None:
            return []
        return _aggregate_policies_from_rust(self._planner.aggregate_policies_json())

    def get_pgn_policies(self) -> list[PgnPolicy]:
        if self._planner is None:
            return []
        return _pgn_policies_from_rust(self._planner.pgn_policies_json())

    def delete_policy(
        self, sources=None, sink=None, constraint="", on_fail=None, description=None
    ) -> bool:
        on_fail_value = on_fail.value if isinstance(on_fail, Resolution) else on_fail
        if self._planner is None:
            return False
        return self._planner.delete_policy(
            sources,
            sink,
            constraint or None,
            on_fail_value,
            description,
        )

    def transform_query(
        self, query: str, use_partial_push: bool = False, collect_stats: bool = False
    ) -> str:
        if self._planner is None:
            return query
        if not self._planner.has_registered_policies():
            return self._planner.transform_query(query)
        return self._planner.transform_registered(query, use_partial_push, collect_stats)

    def last_rewrite_stats(self):
        if self._planner is None:
            return None
        return self._planner.last_rewrite_stats()

    def last_statement_rewrite_summary(self):
        if self._planner is None:
            return None
        return self._planner.last_statement_rewrite_summary()

    def explain_rewrite(self, query: str) -> str:
        if self._planner is None:
            return json.dumps({"chosen": {"rewritten_sql": query}}, indent=2)
        if not self._planner.has_registered_policies():
            return self._planner.explain_rewrite(query)
        return self._planner.explain_rewrite_registered(query)

    def execute(self, query: str, use_partial_push: bool = False):
        rewritten = self.transform_query(query, use_partial_push=use_partial_push)
        executable = _strip_passant_comment(rewritten)
        return self.conn.execute(executable)

    def fetchall(self, query: str, use_partial_push: bool = False):
        return self.execute(query, use_partial_push=use_partial_push).fetchall()

    def fetchone(self, query: str, use_partial_push: bool = False):
        return self.execute(query, use_partial_push=use_partial_push).fetchone()

    def finalize_aggregate_policies(self, sink_table: str) -> dict[str, str | None]:
        policies = [
            policy
            for policy in self.get_aggregate_policies()
            if policy.sink is None or policy.sink == sink_table
        ]
        if not policies:
            return {}
        if not self._table_exists(sink_table):
            return {f"aggregate::{policy.constraint}": None for policy in policies}
        if self._planner is None:
            return {f"aggregate::{policy.constraint}": None for policy in policies}

        query_specs = json.loads(self._planner.aggregate_finalization_registered(sink_table))
        sink_columns = self._get_table_columns(sink_table)
        can_invalidate = sink_columns.get("valid") == "BOOLEAN"
        violations: dict[str, str | None] = {}
        for spec in query_specs:
            policy_id = spec["policy_id"]
            try:
                rows = self.conn.execute(spec["sql"]).fetchall()
                passed = all(bool(row[-1]) if row and row[-1] is not None else True for row in rows)
                if can_invalidate and spec.get("invalidate_sql"):
                    self.conn.execute(spec["invalidate_sql"])
                if passed:
                    violations[policy_id] = None
                else:
                    message = f"Aggregate policy constraint violated: {spec['constraint']}"
                    if spec.get("description"):
                        message = f"{spec['description']}: {message}"
                    violations[policy_id] = message
            except duckdb.Error as exc:
                violations[policy_id] = f"Error evaluating aggregate policy constraint: {exc}"
        return violations

    def _sync_catalog_to_rust(self) -> None:
        if self._planner is None:
            return
        tables: dict[str, dict] = {}
        for table_name in self._list_catalog_tables():
            column_types = self._get_table_columns(table_name)
            tables[table_name] = {
                "columns": list(column_types.keys()),
                "types": column_types,
            }
        snapshot = {"tables": tables, "unique_columns": []}
        self._planner.sync_catalog(json.dumps(snapshot))

    def _list_catalog_tables(self) -> list[str]:
        try:
            rows = self.conn.execute(
                "SELECT schema_name, table_name FROM duckdb_tables() "
                "WHERE NOT internal AND NOT temporary"
            ).fetchall()
        except duckdb.Error:
            rows = [("main", name) for (name,) in self.conn.execute("SHOW TABLES").fetchall()]
        tables: list[str] = []
        for schema, name in rows:
            if schema and schema.lower() not in ("main", "temp"):
                tables.append(f"{schema}.{name}")
            else:
                tables.append(name)
        return tables

    def _register_policy_in_rust(self, policy: DFCPolicy | AggregateDFCPolicy) -> None:
        if self._planner is None:
            return
        dfc_policies = [policy] if isinstance(policy, DFCPolicy) else []
        aggregate_policies = [policy] if isinstance(policy, AggregateDFCPolicy) else []
        policies_json, aggregate_policies_json = self._policy_json(dfc_policies, aggregate_policies)
        self._planner.register_policy_specs(policies_json, aggregate_policies_json)

    def _table_exists(self, table_name: str) -> bool:
        rows = self.conn.execute("SHOW TABLES").fetchall()
        return any(row[0].lower() == table_name.lower() for row in rows)

    def _get_table_columns(self, table_name: str) -> dict[str, str]:
        try:
            rows = self.conn.execute(f"DESCRIBE {_quote_sql_identifier(table_name)}").fetchall()
        except duckdb.Error as exc:
            raise ValueError(f"Table '{table_name}' does not exist") from exc
        return {row[0]: str(row[1]).upper() for row in rows}

    def _get_table_column_names(self, table_name: str) -> list[str]:
        try:
            rows = self.conn.execute(f"DESCRIBE {_quote_sql_identifier(table_name)}").fetchall()
        except duckdb.Error as exc:
            raise ValueError(f"Table '{table_name}' does not exist") from exc
        return [row[0] for row in rows]

    def _policy_json(
        self,
        dfc_policies: list[DFCPolicy],
        aggregate_policies: list[AggregateDFCPolicy],
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

    def get_stream_file_path(self):
        return self.stream_file_path

    def reset_stream_file_path(self) -> None:
        self.stream_file_path = tempfile.NamedTemporaryFile(
            mode="w", delete=False, suffix=".txt"
        ).name

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


def _quote_sql_identifier(name: str) -> str:
    """Quote a DuckDB table identifier, including schema-qualified names."""
    name = name.strip()
    if not name:
        raise ValueError("Table name must be non-empty")

    def quote_part(part: str) -> str:
        part = part.strip()
        if part.startswith('"') and part.endswith('"'):
            return part
        escaped = part.replace('"', '""')
        return f'"{escaped}"'

    return ".".join(quote_part(part) for part in name.split("."))


def _parse_policy_with_rust(policy_str: str) -> dict:
    if _passant is None:
        raise ValueError("Rust Passant extension is not available")
    return json.loads(_passant.parse_policy_to_json(policy_str))


def _resolution_to_python(value) -> str:
    if isinstance(value, str):
        return value.upper()
    mapping = {
        "Remove": "REMOVE",
        "Kill": "KILL",
        "Invalidate": "INVALIDATE",
        "InvalidateMessage": "INVALIDATE_MESSAGE",
        "Llm": "LLM",
    }
    return mapping.get(str(value), str(value).upper())


def _validate_constraint_expression(sql: str, label: str) -> None:
    if _passant is None:
        raise RuntimeError("Passant Rust extension is not built")
    _passant.validate_constraint_expression_py(sql, label)


def _dfc_policies_from_rust(policies_json: str) -> list[DFCPolicy]:
    policies: list[DFCPolicy] = []
    for entry in json.loads(policies_json):
        if "CompatDfc" not in entry:
            continue
        spec = entry["CompatDfc"]
        policies.append(
            DFCPolicy(
                constraint=spec["constraint"],
                on_fail=Resolution(_resolution_to_python(spec["on_fail"])),
                sources=spec["sources"],
                required_sources=spec.get("required_sources", []),
                sink=spec.get("sink"),
                sink_alias=spec.get("sink_alias"),
                description=spec.get("description"),
                dimensions=spec.get("dimensions", []),
            )
        )
    return policies


def _aggregate_policies_from_rust(policies_json: str) -> list[AggregateDFCPolicy]:
    policies: list[AggregateDFCPolicy] = []
    for entry in json.loads(policies_json):
        if "CompatAggregate" not in entry:
            continue
        spec = entry["CompatAggregate"]
        policies.append(
            AggregateDFCPolicy(
                constraint=spec["constraint"],
                on_fail=Resolution.INVALIDATE,
                sources=spec["sources"],
                sink=spec.get("sink"),
                description=spec.get("description"),
                dimensions=spec.get("dimensions", []),
            )
        )
    return policies


def _pgn_policies_from_rust(policies_json: str) -> list[PgnPolicy]:
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


def _normalize_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        raise ValueError("Sources must be provided (use an empty list for no sources)")
    if not isinstance(sources, list):
        raise ValueError("Sources must be provided as a list of table names")
    if _passant is None:
        raise RuntimeError("Passant Rust extension is not built")
    try:
        return _passant.normalize_policy_sources_py(sources)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc


def _normalize_optional_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        return []
    return _normalize_sources(sources)


def _normalize_optional_dimensions(dimensions: list[str] | None) -> list[str]:
    if dimensions is None:
        return []
    if not isinstance(dimensions, list):
        raise ValueError("Dimensions must be provided as a list of qualified column names")
    if _passant is None:
        raise RuntimeError("Passant Rust extension is not built")
    try:
        return _passant.normalize_policy_dimensions_py(dimensions)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc
