"""CallToolWithDataFlow LangChain tool implementation."""

from __future__ import annotations

import asyncio
import json
from typing import Any

from langchain.tools import tool

from .database import ToolUseDatabase
from .middleware import CALL_TOOL_WITH_DATAFLOW_NAME
from .sql_validation import validate_call_tool_sql


class CallToolWithDataFlowRunner:
    """Executes SQL over Passant tool tables and drains pending tool inputs."""

    def __init__(self, db: ToolUseDatabase) -> None:
        self.db = db

    def _drain_new_rows(self, before: dict[str, set[int]]) -> list[dict[str, Any]]:
        tool_outputs: list[dict[str, Any]] = []
        new_rows = self.db.find_new_pending_rows(before)
        with self.db._lock:
            for tool_name, input_id in new_rows:
                if not self.db.claim_input_row(tool_name, input_id):
                    continue
                status, _result, output_id = self.db.execute_tool_for_input(tool_name, input_id)
                tool_outputs.append(
                    {
                        "tool": tool_name,
                        "input_id": input_id,
                        "status": status,
                        "output_id": output_id,
                    }
                )
        return tool_outputs

    async def _adrain_new_rows(self, before: dict[str, set[int]]) -> list[dict[str, Any]]:
        tool_outputs: list[dict[str, Any]] = []
        new_rows = self.db.find_new_pending_rows(before)
        if self.db._async_lock is None:
            self.db._async_lock = asyncio.Lock()
        async with self.db._async_lock:
            for tool_name, input_id in new_rows:
                if not self.db.claim_input_row(tool_name, input_id):
                    continue
                status, _result, output_id = await self.db.execute_tool_for_input_async(
                    tool_name, input_id
                )
                tool_outputs.append(
                    {
                        "tool": tool_name,
                        "input_id": input_id,
                        "status": status,
                        "output_id": output_id,
                    }
                )
        return tool_outputs

    def run(self, sql: str) -> str:
        validated = validate_call_tool_sql(
            sql,
            allowed_input_tables=self.db.allowed_input_tables,
            protected_tables=self.db.protected_tables,
        )
        sql_call_id = self.db._next_sql_call_id()
        before = self.db.snapshot_pending_input_ids()
        rows: list[list[Any]] = []
        error: str | None = None
        tool_outputs: list[dict[str, Any]] = []

        try:
            if validated.statement_type == "select":
                result = self.db.conn.execute(validated.sql)
                rows = [list(row) for row in result.fetchall()]
            else:
                self.db.conn.execute(validated.sql)

            tool_outputs = self._drain_new_rows(before)
        except Exception as exc:
            error = str(exc)

        self.db.log_sql_call(
            sql_call_id=sql_call_id,
            sql=validated.sql,
            row_count_inserted=len(tool_outputs),
            tool_calls_executed=len(tool_outputs),
            error=error,
        )

        if error is not None:
            raise RuntimeError(error)

        payload = {
            "sql_call_id": sql_call_id,
            "statement_type": validated.statement_type,
            "inserted_input_rows": len(tool_outputs),
            "tool_calls_executed": len(tool_outputs),
            "tool_outputs": tool_outputs,
            "rows": rows,
        }
        return json.dumps(payload, default=str)

    async def arun(self, sql: str) -> str:
        validated = validate_call_tool_sql(
            sql,
            allowed_input_tables=self.db.allowed_input_tables,
            protected_tables=self.db.protected_tables,
        )
        sql_call_id = self.db._next_sql_call_id()
        before = self.db.snapshot_pending_input_ids()
        rows: list[list[Any]] = []
        error: str | None = None
        tool_outputs: list[dict[str, Any]] = []

        try:
            if validated.statement_type == "select":
                result = await asyncio.to_thread(self.db.conn.execute, validated.sql)
                rows = [list(row) for row in result.fetchall()]
            else:
                await asyncio.to_thread(self.db.conn.execute, validated.sql)

            tool_outputs = await self._adrain_new_rows(before)
        except Exception as exc:
            error = str(exc)

        await asyncio.to_thread(
            self.db.log_sql_call,
            sql_call_id=sql_call_id,
            sql=validated.sql,
            row_count_inserted=len(tool_outputs),
            tool_calls_executed=len(tool_outputs),
            error=error,
        )

        if error is not None:
            raise RuntimeError(error)

        payload = {
            "sql_call_id": sql_call_id,
            "statement_type": validated.statement_type,
            "inserted_input_rows": len(tool_outputs),
            "tool_calls_executed": len(tool_outputs),
            "tool_outputs": tool_outputs,
            "rows": rows,
        }
        return json.dumps(payload, default=str)


def build_call_tool_with_dataflow(db: ToolUseDatabase) -> Any:
    runner = CallToolWithDataFlowRunner(db)

    @tool(CALL_TOOL_WITH_DATAFLOW_NAME)
    def call_tool_with_dataflow(sql: str) -> str:
        """Execute SQL over Passant tool input/output tables and run inserted tool inputs."""
        return runner.run(sql)

    return call_tool_with_dataflow
