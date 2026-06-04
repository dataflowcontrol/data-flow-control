"""Passant-managed DuckDB tables for LangChain tool I/O."""

from __future__ import annotations

import json
import threading
from typing import Any, Literal

import duckdb

from data_flow_control.connection import dfc
from data_flow_control.policy import Policy
from data_flow_control.connection import Connection

from .schema import (
    input_table_name,
    normalize_tool_names,
    output_table_name,
    quote_identifier,
    schema_to_json,
)
from .sql_validation import validate_debug_fetch_sql
from .tool_spec import ToolSpec

DirectToolMode = Literal["observe", "enforce"]

_DIRECT_TOOL_MODES = frozenset({"observe", "enforce"})


def parse_direct_tool_mode(value: str) -> DirectToolMode:
    if value not in _DIRECT_TOOL_MODES:
        raise ValueError(f"direct_tool_mode must be 'observe' or 'enforce', got {value!r}")
    return value


class ToolUseDatabase:
    """Owns a DuckDB connection and Passant tool input/output tables."""

    def __init__(
        self,
        specs: list[ToolSpec],
        *,
        db_path: str | None = None,
        direct_tool_mode: DirectToolMode = "observe",
        policies: list[Policy] | None = None,
    ) -> None:
        self._lock = threading.Lock()
        self._async_lock: Any | None = None
        self.direct_tool_mode = parse_direct_tool_mode(direct_tool_mode)
        self.specs = specs
        self.spec_by_name = {spec.name: spec for spec in specs}
        self.table_base_by_tool = normalize_tool_names([spec.name for spec in specs])
        self.input_table_by_tool = {name: input_table_name(name) for name in self.spec_by_name}
        self.output_table_by_tool = {name: output_table_name(name) for name in self.spec_by_name}
        self._raw = duckdb.connect(database=db_path or ":memory:")
        self.conn: Connection = dfc(self._raw)
        self._create_internal_tables()
        self._create_tool_tables()
        self.conn.refresh_catalog(force=True)
        if policies:
            self.register_policies(policies)

    @property
    def protected_tables(self) -> set[str]:
        tables = {"__passant_tool_registry", "__passant_tool_sql_calls"}
        for spec in self.specs:
            tables.add(self.output_table_by_tool[spec.name])
        return tables

    @property
    def allowed_input_tables(self) -> set[str]:
        return set(self.input_table_by_tool.values())

    def register_policy(self, policy: Policy) -> None:
        self.register_policies([policy])

    def register_policies(self, policies: list[Policy]) -> None:
        self.conn.refresh_catalog(force=True)
        self.conn.register_policies(policies)

    def fetchall(self, sql: str) -> list[tuple]:
        validated = validate_debug_fetch_sql(sql)
        return self._fetchall_raw(validated)

    def close(self) -> None:
        self.conn.close()

    def _execute_raw(self, sql: str, params: list[Any] | None = None):
        return self._raw.execute(sql, params or [])

    def _fetchall_raw(self, sql: str, params: list[Any] | None = None) -> list[tuple]:
        return self._execute_raw(sql, params).fetchall()

    def _fetchone_raw(self, sql: str, params: list[Any] | None = None) -> tuple | None:
        return self._execute_raw(sql, params).fetchone()

    def _create_internal_tables(self) -> None:
        self._execute_raw(
            """
            CREATE SEQUENCE IF NOT EXISTS __passant_tool_call_seq START 1
            """
        )
        self._execute_raw(
            """
            CREATE TABLE IF NOT EXISTS __passant_tool_registry (
              tool_name VARCHAR PRIMARY KEY,
              input_table VARCHAR NOT NULL,
              output_table VARCHAR NOT NULL,
              input_schema_json JSON NOT NULL,
              output_schema_json JSON,
              direct_tool_mode VARCHAR NOT NULL,
              created_at TIMESTAMP DEFAULT current_timestamp
            )
            """
        )
        self._execute_raw(
            """
            CREATE TABLE IF NOT EXISTS __passant_tool_sql_calls (
              sql_call_id BIGINT PRIMARY KEY,
              sql TEXT NOT NULL,
              created_at TIMESTAMP DEFAULT current_timestamp,
              row_count_inserted BIGINT DEFAULT 0,
              tool_calls_executed BIGINT DEFAULT 0,
              error TEXT
            )
            """
        )

    def _create_tool_tables(self) -> None:
        for spec in self.specs:
            in_table = self.input_table_by_tool[spec.name]
            out_table = self.output_table_by_tool[spec.name]
            self._create_input_table(spec, in_table)
            self._create_output_table(spec, out_table)
            self._execute_raw(
                """
                INSERT INTO __passant_tool_registry (
                  tool_name, input_table, output_table,
                  input_schema_json, output_schema_json, direct_tool_mode
                ) VALUES (?, ?, ?, ?, ?, ?)
                """,
                [
                    spec.name,
                    in_table,
                    out_table,
                    schema_to_json(spec.input_schema),
                    schema_to_json(spec.output_schema) if spec.output_schema else None,
                    self.direct_tool_mode,
                ],
            )

    def _create_input_table(self, spec: ToolSpec, table_name: str) -> None:
        user_columns = ",\n  ".join(
            f"{quote_identifier(name)} {dtype}" for name, dtype in spec.input_schema.items()
        )
        user_sql = f",\n  {user_columns}" if user_columns else ""
        ddl = f"""
            CREATE TABLE {quote_identifier(table_name)} (
              __passant_input_id BIGINT DEFAULT nextval('__passant_tool_call_seq') PRIMARY KEY,
              __passant_tool_name VARCHAR DEFAULT '{spec.name}',
              __passant_tool_call_id VARCHAR,
              __passant_origin VARCHAR DEFAULT 'dataflow_sql',
              __passant_sql_call_id BIGINT,
              __passant_thread_id VARCHAR,
              __passant_run_id VARCHAR,
              __passant_created_at TIMESTAMP DEFAULT current_timestamp,
              __passant_executed_at TIMESTAMP,
              __passant_status VARCHAR DEFAULT 'pending',
              __passant_error TEXT{user_sql}
            )
            """
        self._execute_raw(ddl)

    def _create_output_table(self, spec: ToolSpec, table_name: str) -> None:
        user_columns = ",\n  ".join(
            f"{quote_identifier(name)} {dtype}" for name, dtype in spec.output_schema.items()
        )
        user_sql = f",\n  {user_columns}" if user_columns else ""
        ddl = f"""
            CREATE TABLE {quote_identifier(table_name)} (
              __passant_output_id BIGINT DEFAULT nextval('__passant_tool_call_seq') PRIMARY KEY,
              __passant_input_id BIGINT NOT NULL,
              __passant_tool_name VARCHAR DEFAULT '{spec.name}',
              __passant_tool_call_id VARCHAR,
              __passant_thread_id VARCHAR,
              __passant_run_id VARCHAR,
              __passant_created_at TIMESTAMP DEFAULT current_timestamp,
              __passant_status VARCHAR NOT NULL,
              __passant_error TEXT,
              __passant_raw_json JSON{user_sql}
            )
            """
        self._execute_raw(ddl)

    def _next_sql_call_id(self) -> int:
        row = self._fetchone_raw(
            "SELECT COALESCE(MAX(sql_call_id), 0) + 1 FROM __passant_tool_sql_calls"
        )
        return int(row[0])

    def snapshot_pending_input_ids(self) -> dict[str, set[int]]:
        pending: dict[str, set[int]] = {}
        for tool_name, table in self.input_table_by_tool.items():
            rows = self._fetchall_raw(
                f"""
                SELECT __passant_input_id
                FROM {quote_identifier(table)}
                WHERE __passant_status = 'pending'
                """
            )
            pending[tool_name] = {int(row[0]) for row in rows}
        return pending

    def find_new_pending_rows(
        self,
        before: dict[str, set[int]],
    ) -> list[tuple[str, int]]:
        after = self.snapshot_pending_input_ids()
        new_rows: list[tuple[str, int]] = []
        for tool_name, ids in after.items():
            previous = before.get(tool_name, set())
            for input_id in sorted(ids - previous):
                new_rows.append((tool_name, input_id))
        new_rows.sort(key=lambda item: item[1])
        return new_rows

    def _read_input_row(self, tool_name: str, input_id: int) -> dict[str, Any]:
        table = self.input_table_by_tool[tool_name]
        spec = self.spec_by_name[tool_name]
        columns = ["__passant_input_id", *_user_columns(spec.input_schema)]
        col_sql = ", ".join(quote_identifier(name) for name in columns)
        row = self._fetchone_raw(
            f"SELECT {col_sql} FROM {quote_identifier(table)} WHERE __passant_input_id = ?",
            [input_id],
        )
        if row is None:
            raise ValueError(f"Missing input row {input_id} for tool {tool_name!r}")
        return dict(zip(columns, row, strict=True))

    def claim_input_row(self, tool_name: str, input_id: int) -> bool:
        table = self.input_table_by_tool[tool_name]
        self._execute_raw(
            f"""
            UPDATE {quote_identifier(table)}
            SET __passant_status = 'running', __passant_executed_at = current_timestamp
            WHERE __passant_input_id = ? AND __passant_status = 'pending'
            """,
            [input_id],
        )
        row = self._fetchone_raw(
            f"""
            SELECT __passant_status FROM {quote_identifier(table)}
            WHERE __passant_input_id = ?
            """,
            [input_id],
        )
        return row is not None and row[0] == "running"

    def mark_input_status(
        self,
        tool_name: str,
        input_id: int,
        *,
        status: str,
        error: str | None = None,
    ) -> None:
        table = self.input_table_by_tool[tool_name]
        self._execute_raw(
            f"""
            UPDATE {quote_identifier(table)}
            SET __passant_status = ?, __passant_error = ?, __passant_executed_at = current_timestamp
            WHERE __passant_input_id = ?
            """,
            [status, error, input_id],
        )

    def insert_direct_input_row(
        self,
        tool_name: str,
        args: dict[str, Any],
        *,
        tool_call_id: str | None = None,
        thread_id: str | None = None,
        run_id: str | None = None,
    ) -> int:
        table = self.input_table_by_tool[tool_name]
        spec = self.spec_by_name[tool_name]
        user_cols = list(spec.input_schema.keys())
        columns = [
            "__passant_origin",
            "__passant_tool_call_id",
            "__passant_thread_id",
            "__passant_run_id",
            "__passant_status",
            *user_cols,
        ]
        values = [
            "direct",
            tool_call_id,
            thread_id,
            run_id,
            "running",
            *[args.get(col) for col in user_cols],
        ]
        placeholders = ", ".join("?" for _ in columns)
        col_sql = ", ".join(quote_identifier(name) for name in columns)
        self._execute_raw(
            f"INSERT INTO {quote_identifier(table)} ({col_sql}) VALUES ({placeholders})",
            values,
        )
        row = self._fetchone_raw(f"SELECT MAX(__passant_input_id) FROM {quote_identifier(table)}")
        return int(row[0])

    def insert_output_row(
        self,
        tool_name: str,
        *,
        input_id: int,
        status: str,
        payload: Any,
        error: str | None = None,
        tool_call_id: str | None = None,
        thread_id: str | None = None,
        run_id: str | None = None,
    ) -> int:
        table = self.output_table_by_tool[tool_name]
        spec = self.spec_by_name[tool_name]
        raw_json = _serialize_payload(payload)
        scalar_values = _extract_scalar_fields(spec.output_schema, payload)
        user_cols = list(spec.output_schema.keys())
        columns = [
            "__passant_input_id",
            "__passant_tool_call_id",
            "__passant_thread_id",
            "__passant_run_id",
            "__passant_status",
            "__passant_error",
            "__passant_raw_json",
            *user_cols,
        ]
        values = [
            input_id,
            tool_call_id,
            thread_id,
            run_id,
            status,
            error,
            raw_json,
            *[scalar_values.get(col) for col in user_cols],
        ]
        placeholders = ", ".join("?" for _ in columns)
        col_sql = ", ".join(quote_identifier(name) for name in columns)
        self._execute_raw(
            f"INSERT INTO {quote_identifier(table)} ({col_sql}) VALUES ({placeholders})",
            values,
        )
        row = self._fetchone_raw(f"SELECT MAX(__passant_output_id) FROM {quote_identifier(table)}")
        return int(row[0])

    def execute_tool_for_input(self, tool_name: str, input_id: int) -> tuple[str, Any, int]:
        spec = self.spec_by_name[tool_name]
        row = self._read_input_row(tool_name, input_id)
        kwargs = {col: row.get(col) for col in spec.input_schema if row.get(col) is not None}
        try:
            if spec.call_sync is None:
                raise ValueError(f"Tool {tool_name!r} does not support sync execution")
            result = spec.call_sync(**kwargs)
            output_id = self.insert_output_row(
                tool_name,
                input_id=input_id,
                status="succeeded",
                payload=result,
                tool_call_id=row.get("__passant_tool_call_id"),
                thread_id=row.get("__passant_thread_id"),
                run_id=row.get("__passant_run_id"),
            )
            self.mark_input_status(tool_name, input_id, status="succeeded")
            return "succeeded", result, output_id
        except Exception as exc:
            output_id = self.insert_output_row(
                tool_name,
                input_id=input_id,
                status="failed",
                payload=None,
                error=str(exc),
                tool_call_id=row.get("__passant_tool_call_id"),
                thread_id=row.get("__passant_thread_id"),
                run_id=row.get("__passant_run_id"),
            )
            self.mark_input_status(tool_name, input_id, status="failed", error=str(exc))
            return "failed", None, output_id

    async def execute_tool_for_input_async(
        self, tool_name: str, input_id: int
    ) -> tuple[str, Any, int]:
        spec = self.spec_by_name[tool_name]
        row = self._read_input_row(tool_name, input_id)
        kwargs = {col: row.get(col) for col in spec.input_schema if row.get(col) is not None}
        try:
            if spec.call_async is None:
                raise ValueError(f"Tool {tool_name!r} does not support async execution")
            result = await spec.call_async(**kwargs)
            output_id = self.insert_output_row(
                tool_name,
                input_id=input_id,
                status="succeeded",
                payload=result,
                tool_call_id=row.get("__passant_tool_call_id"),
                thread_id=row.get("__passant_thread_id"),
                run_id=row.get("__passant_run_id"),
            )
            self.mark_input_status(tool_name, input_id, status="succeeded")
            return "succeeded", result, output_id
        except Exception as exc:
            output_id = self.insert_output_row(
                tool_name,
                input_id=input_id,
                status="failed",
                payload=None,
                error=str(exc),
                tool_call_id=row.get("__passant_tool_call_id"),
                thread_id=row.get("__passant_thread_id"),
                run_id=row.get("__passant_run_id"),
            )
            self.mark_input_status(tool_name, input_id, status="failed", error=str(exc))
            return "failed", None, output_id

    def log_sql_call(
        self,
        *,
        sql_call_id: int,
        sql: str,
        row_count_inserted: int,
        tool_calls_executed: int,
        error: str | None = None,
    ) -> None:
        self._execute_raw(
            """
            INSERT INTO __passant_tool_sql_calls (
              sql_call_id, sql, row_count_inserted, tool_calls_executed, error
            ) VALUES (?, ?, ?, ?, ?)
            """,
            [sql_call_id, sql, row_count_inserted, tool_calls_executed, error],
        )


def _user_columns(schema: dict[str, str]) -> list[str]:
    return list(schema.keys())


def _serialize_payload(payload: Any) -> str | None:
    if payload is None:
        return None
    if hasattr(payload, "model_dump"):
        payload = payload.model_dump()
    return json.dumps(payload, default=str)


def _extract_scalar_fields(schema: dict[str, str], payload: Any) -> dict[str, Any]:
    if payload is None:
        return {}
    if hasattr(payload, "model_dump"):
        payload = payload.model_dump()
    if not isinstance(payload, dict):
        return {}
    return {key: payload.get(key) for key in schema}
