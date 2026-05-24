from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
import json
import re
import tempfile

import duckdb
import sqlglot
from sqlglot import exp

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
        _validate_sql_expression(self.constraint, "constraint")
        _validate_qualified_columns(self.constraint)
        for dimension in self.dimensions:
            _validate_sql_expression(dimension, "dimension")
            _validate_qualified_columns(dimension)

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
        _validate_sql_expression(self.constraint, "constraint")
        _validate_qualified_columns(self.constraint)
        for dimension in self.dimensions:
            _validate_sql_expression(dimension, "dimension")
            _validate_qualified_columns(dimension)

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
    def __init__(self, conn=None, stream_file_path=None, bedrock_client=None, bedrock_model_id=None, recorder=None):
        self.conn = conn or duckdb.connect()
        self.stream_file_path = stream_file_path or tempfile.NamedTemporaryFile(
            mode="w", delete=False, suffix=".txt"
        ).name
        self.bedrock_client = bedrock_client
        self.bedrock_model_id = bedrock_model_id
        self.recorder = recorder
        self._resolver_functions = {}
        self._policies: list[DFCPolicy | AggregateDFCPolicy | PgnPolicy] = []
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
            self._validate_policy_catalog(policy)
            self._register_policy_in_rust(policy)
        elif isinstance(policy, PgnPolicy):
            if self._planner is not None:
                self._planner.register_policy_text(policy.text)
        self._policies.append(policy)

    def get_dfc_policies(self) -> list[DFCPolicy]:
        policies = [policy for policy in self._policies if isinstance(policy, DFCPolicy)]
        self._assert_rust_policy_count("dfc", len(policies))
        return policies

    def get_aggregate_policies(self) -> list[AggregateDFCPolicy]:
        policies = [policy for policy in self._policies if isinstance(policy, AggregateDFCPolicy)]
        self._assert_rust_policy_count("aggregate", len(policies))
        return policies

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
            if self._planner is not None and isinstance(policy, DFCPolicy | AggregateDFCPolicy):
                deleted = self._planner.delete_policy(
                    sources,
                    sink,
                    constraint or None,
                    on_fail.value if isinstance(on_fail, Resolution) else on_fail,
                    description,
                )
                if not deleted:
                    return False
            del self._policies[idx]
            return True
        return False

    def transform_query(self, query: str, use_two_phase: bool = False) -> str:
        _ = use_two_phase
        if self._planner is None:
            return query
        dfc_policies = self.get_dfc_policies()
        aggregate_policies = self.get_aggregate_policies()
        if not dfc_policies and not aggregate_policies:
            return self._planner.transform_query(query)
        query = self._expand_insert_columns_from_catalog(query)
        return self._planner.transform_registered(query)

    def explain_rewrite(self, query: str) -> str:
        if self._planner is None:
            return json.dumps({"chosen": {"rewritten_sql": query}}, indent=2)
        dfc_policies = self.get_dfc_policies()
        aggregate_policies = self.get_aggregate_policies()
        if not dfc_policies and not aggregate_policies:
            return self._planner.explain_rewrite(query)
        query = self._expand_insert_columns_from_catalog(query)
        return self._planner.explain_rewrite_registered(query)

    def execute(self, query: str, use_two_phase: bool = False):
        rewritten = self.transform_query(query, use_two_phase=use_two_phase)
        executable = _strip_passant_comment(rewritten)
        return self.conn.execute(executable)

    def fetchall(self, query: str, use_two_phase: bool = False):
        return self.execute(query, use_two_phase=use_two_phase).fetchall()

    def fetchone(self, query: str, use_two_phase: bool = False):
        return self.execute(query, use_two_phase=use_two_phase).fetchone()

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

    def _register_policy_in_rust(self, policy: DFCPolicy | AggregateDFCPolicy) -> None:
        if self._planner is None:
            return
        dfc_policies = [policy] if isinstance(policy, DFCPolicy) else []
        aggregate_policies = [policy] if isinstance(policy, AggregateDFCPolicy) else []
        policies_json, aggregate_policies_json = self._policy_json(dfc_policies, aggregate_policies)
        self._planner.register_policy_specs(policies_json, aggregate_policies_json)

    def _assert_rust_policy_count(self, policy_type: str, expected: int) -> None:
        if self._planner is None:
            return
        if policy_type == "dfc":
            actual = len(json.loads(self._planner.dfc_policies_json()))
        else:
            actual = len(json.loads(self._planner.aggregate_policies_json()))
        if actual != expected:
            raise RuntimeError(
                f"Rust policy storage mismatch for {policy_type} policies: "
                f"expected {expected}, found {actual}"
            )

    def _table_exists(self, table_name: str) -> bool:
        rows = self.conn.execute("SHOW TABLES").fetchall()
        return any(row[0].lower() == table_name.lower() for row in rows)

    def _get_table_columns(self, table_name: str) -> dict[str, str]:
        try:
            rows = self.conn.execute(f"DESCRIBE {table_name}").fetchall()
        except duckdb.Error as exc:
            raise ValueError(f"Table '{table_name}' does not exist") from exc
        return {row[0].lower(): str(row[1]).upper() for row in rows}

    def _get_table_column_names(self, table_name: str) -> list[str]:
        try:
            rows = self.conn.execute(f"DESCRIBE {table_name}").fetchall()
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

    def _expand_insert_columns_from_catalog(self, query: str) -> str:
        try:
            parsed = sqlglot.parse_one(query, read="duckdb")
        except sqlglot.errors.ParseError:
            return query
        if not isinstance(parsed, exp.Insert) or isinstance(parsed.this, exp.Schema):
            return query
        if not isinstance(parsed.this, exp.Table):
            return query
        expression = parsed.args.get("expression")
        if not isinstance(expression, exp.Select):
            return query

        columns = self._get_table_column_names(parsed.this.name)
        parsed.set(
            "this",
            exp.Schema(
                this=parsed.this.copy(),
                expressions=[exp.to_identifier(column) for column in columns],
            ),
        )
        return parsed.sql(dialect="duckdb")

    def _validate_policy_catalog(self, policy: DFCPolicy | AggregateDFCPolicy) -> None:
        source_columns = {}
        for source in policy.sources:
            if not self._table_exists(source):
                raise ValueError(f"Source table '{source}' does not exist")
            source_columns[source.lower()] = self._get_table_columns(source)

        sink_columns = None
        if policy.sink:
            if not self._table_exists(policy.sink):
                raise ValueError(f"Sink table '{policy.sink}' does not exist")
            sink_columns = self._get_table_columns(policy.sink)

        if isinstance(policy, DFCPolicy) and policy.sink and policy.on_fail == Resolution.INVALIDATE:
            valid_type = (sink_columns or {}).get("valid")
            if valid_type != "BOOLEAN":
                raise ValueError(
                    f"Sink table '{policy.sink}' must have a boolean column named 'valid' "
                    "for INVALIDATE resolution policies"
                )

        if (
            isinstance(policy, DFCPolicy)
            and policy.sink
            and policy.on_fail == Resolution.INVALIDATE_MESSAGE
        ):
            invalid_type = (sink_columns or {}).get("invalid_string", "")
            if not any(token in invalid_type for token in ("CHAR", "VARCHAR", "STRING", "TEXT")):
                raise ValueError(
                    f"Sink table '{policy.sink}' must have a string column named "
                    "'invalid_string' for INVALIDATE_MESSAGE resolution policies"
                )

        source_names = {source.lower() for source in policy.sources}
        sink_names = {policy.sink.lower()} if policy.sink else set()
        if policy.sink:
            sink_names.add("_output_")
        if isinstance(policy, DFCPolicy) and policy.sink_alias:
            sink_names.add(policy.sink_alias.lower())

        referenced_columns = _qualified_columns(policy.constraint)
        if isinstance(policy, DFCPolicy | AggregateDFCPolicy):
            for dimension in policy.dimensions:
                referenced_columns.extend(_qualified_columns(dimension))

        for table_name, column_name in referenced_columns:
            table_key = table_name.lower()
            column_key = column_name.lower()
            if table_key in source_names:
                if column_key not in source_columns[table_key]:
                    raise ValueError(
                        f"Column '{table_name}.{column_name}' referenced in constraint "
                        f"does not exist in source table '{table_name}'"
                    )
            elif table_key in sink_names:
                if sink_columns is not None and column_key not in sink_columns:
                    raise ValueError(
                        f"Column '{table_name}.{column_name}' referenced in constraint "
                        f"does not exist in sink table '{policy.sink}'"
                    )
            else:
                raise ValueError(
                    f"Column '{table_name}.{column_name}' referenced in constraint "
                    f"references table '{table_name}', which is not in sources "
                    f"({policy.sources}) or sink ('{policy.sink}')"
                )

    def get_stream_file_path(self):
        return self.stream_file_path

    def reset_stream_file_path(self) -> None:
        self.stream_file_path = tempfile.NamedTemporaryFile(mode="w", delete=False, suffix=".txt").name

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


def _qualified_columns(sql: str) -> list[tuple[str, str]]:
    return [
        (match.group(1), match.group(2))
        for match in re.finditer(r"\b([A-Za-z_][A-Za-z0-9_]*)\.([A-Za-z_][A-Za-z0-9_]*)\b", sql)
    ]


def _validate_sql_expression(sql: str, label: str) -> None:
    try:
        parsed = sqlglot.parse_one(f"SELECT {sql}", read="duckdb")
    except sqlglot.errors.ParseError as exc:
        raise ValueError(f"Invalid {label} SQL expression '{sql}': {exc}") from exc
    if not isinstance(parsed, exp.Select) or not parsed.expressions:
        raise ValueError(f"Invalid {label} SQL expression '{sql}'")


def _validate_qualified_columns(sql: str) -> None:
    parsed = sqlglot.parse_one(f"SELECT {sql}", read="duckdb")
    unqualified = [
        column.name
        for column in parsed.find_all(exp.Column)
        if not column.table
    ]
    if unqualified:
        raise ValueError(
            "All columns in constraints and dimensions must be qualified with table names. "
            f"Unqualified columns found: {', '.join(unqualified)}"
        )


def _normalize_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        raise ValueError("Sources must be provided (use an empty list for no sources)")
    if not isinstance(sources, list):
        raise ValueError("Sources must be provided as a list of table names")
    normalized = []
    seen = set()
    for source in sources:
        if not isinstance(source, str) or not source.strip():
            raise ValueError("Sources must be non-empty strings")
        key = source.strip().lower()
        if key in seen:
            raise ValueError(f"Duplicate source table '{source.strip()}' in sources list")
        seen.add(key)
        normalized.append(source.strip())
    return normalized


def _normalize_optional_sources(sources: list[str] | None) -> list[str]:
    if sources is None:
        return []
    return _normalize_sources(sources)


def _normalize_optional_dimensions(dimensions: list[str] | None) -> list[str]:
    if dimensions is None:
        return []
    if not isinstance(dimensions, list):
        raise ValueError("Dimensions must be provided as a list of qualified column names")
    normalized = []
    seen = set()
    for dimension in dimensions:
        if not isinstance(dimension, str) or not dimension.strip():
            raise ValueError("Dimensions must be non-empty strings")
        key = dimension.strip().lower()
        if key in seen:
            raise ValueError(f"Duplicate dimension '{dimension.strip()}' in dimensions list")
        seen.add(key)
        normalized.append(dimension.strip())
    return normalized
