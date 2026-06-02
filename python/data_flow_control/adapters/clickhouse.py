from __future__ import annotations

from typing import Any

from ..aggregate_introspection import introspect_clickhouse_aggregates
from ..catalog import build_catalog_snapshot
from .base import Capabilities
from .duckdb import quote_sql_identifier
from .kill import CLICKHOUSE_KILL_DDL

try:
    import clickhouse_connect
except ImportError:  # pragma: no cover
    clickhouse_connect = None


class _ClickHouseCursor:
    def __init__(self, result) -> None:
        self._result = result

    def fetchall(self) -> list[tuple]:
        if self._result.result_rows is None:
            return []
        return [tuple(row) for row in self._result.result_rows]

    def fetchone(self):
        rows = self.fetchall()
        return rows[0] if rows else None


class ClickHouseAdapter:
    dialect = "clickhouse"
    capabilities = Capabilities(exception_udf=True)

    def __init__(self, client) -> None:
        if clickhouse_connect is None:
            raise RuntimeError(
                "ClickHouse support requires clickhouse-connect: uv sync --extra clickhouse"
            )
        self._client = client

    @property
    def client(self):
        return self._client

    def execute(self, sql: str, params: Any = None):
        if params is not None:
            raise ValueError("ClickHouse adapter does not support query parameters")
        return _ClickHouseCursor(self._client.query(sql))

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        self._client.command(CLICKHOUSE_KILL_DDL)

    def register_resolution_function(
        self,
        name: str,
        func: Any,
        parameter_types: list[Any],
        return_type: Any,
    ) -> None:
        raise ValueError(f"Tuple UDF resolution is not supported for dialect {self.dialect!r}")

    def register_relation_resolution_function(self, name: str, func: Any) -> None:
        raise ValueError(f"Relation UDF resolution is not supported for dialect {self.dialect!r}")

    def introspect_catalog(self) -> dict:
        database = getattr(self._client, "database", "default") or "default"
        rows = self._client.query(
            """
            SELECT database, table, name, type
            FROM system.columns
            WHERE database = {database:String}
            ORDER BY database, table, name
            """,
            parameters={"database": database},
        ).result_rows
        tables: dict[str, dict] = {}
        for db_name, table_name, column_name, column_type in rows:
            key = f"{db_name}.{table_name}" if db_name != "default" else table_name
            entry = tables.setdefault(key, {"columns": [], "types": {}})
            entry["columns"].append(column_name)
            entry["types"][column_name] = str(column_type).upper()
        return build_catalog_snapshot(
            dialect=self.dialect,
            tables=tables,
            default_schema=database,
            search_path=[database],
            aggregate_functions=self.introspect_aggregate_functions(),
        )

    def introspect_aggregate_functions(self) -> list[dict]:
        return introspect_clickhouse_aggregates(self._client)

    def close(self) -> None:
        self._client.close()
