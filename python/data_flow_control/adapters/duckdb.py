from __future__ import annotations

from typing import Any

import duckdb

from ..aggregate_introspection import introspect_duckdb_aggregates
from ..catalog import build_catalog_snapshot
from .base import Capabilities
from .kill import python_kill


def quote_sql_identifier(name: str) -> str:
    """Quote a SQL identifier, including schema-qualified names."""
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


class DuckDBAdapter:
    dialect = "duckdb"
    capabilities = Capabilities(
        exception_udf=True,
        tuple_udf=True,
        relation_udf=True,
        ui_resolution=True,
    )

    def __init__(self, conn: duckdb.DuckDBPyConnection) -> None:
        self._conn = conn

    @property
    def connection(self) -> duckdb.DuckDBPyConnection:
        return self._conn

    def execute(self, sql: str, params: Any = None):
        if params is None:
            return self._conn.execute(sql)
        return self._conn.execute(sql, params)

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        for name in ("kill", "passant_kill"):
            try:
                self._conn.create_function(name, python_kill, [], "BOOLEAN")
            except duckdb.Error:
                pass

    def register_resolution_function(
        self,
        name: str,
        func: Any,
        parameter_types: list[Any],
        return_type: Any,
    ) -> None:
        self._conn.create_function(
            name,
            func,
            parameter_types,
            return_type,
            null_handling="special",
        )

    def register_relation_resolution_function(self, name: str, func: Any) -> None:
        """Register a scalar UDF invoked with bool_or(violation) over the relation input."""
        self._conn.create_function(name, func, ["BOOLEAN"], "BOOLEAN")

    def introspect_catalog(self, *, include_row_counts: bool = False) -> dict:
        table_names = self._list_catalog_tables()
        columns_by_table = self._fetch_all_table_columns(table_names)
        primary_keys = self._fetch_all_primary_keys(table_names)
        tables: dict[str, dict] = {}
        unique_columns: list[list[str]] = []
        for table_name in table_names:
            column_types = columns_by_table.get(table_name)
            if column_types is None:
                continue
            entry: dict = {
                "columns": list(column_types.keys()),
                "types": column_types,
            }
            if include_row_counts:
                row_count = self._table_row_count(table_name)
                if row_count is not None:
                    entry["row_count"] = row_count
            tables[table_name] = entry
            for column in primary_keys.get(table_name, []):
                unique_columns.append([table_name, column])
        return build_catalog_snapshot(
            dialect=self.dialect,
            tables=tables,
            default_schema="main",
            search_path=["main"],
            unique_columns=unique_columns,
            aggregate_functions=self.introspect_aggregate_functions(),
        )

    def introspect_aggregate_functions(self) -> list[dict]:
        return introspect_duckdb_aggregates(self._conn)

    def _query_duckdb_columns(self) -> list | None:
        queries = (
            "SELECT database_name, schema_name, table_name, column_name, data_type "
            "FROM duckdb_columns() WHERE NOT internal",
            "SELECT table_catalog, table_schema, table_name, column_name, data_type "
            "FROM duckdb_columns() WHERE NOT internal",
        )
        for sql in queries:
            try:
                return self.execute(sql).fetchall()
            except duckdb.Error:
                continue
        return None

    def _list_catalog_tables(self) -> list[str]:
        try:
            rows = self.execute(
                "SELECT schema_name, table_name FROM duckdb_tables() "
                "WHERE NOT internal AND NOT temporary"
            ).fetchall()
        except duckdb.Error:
            rows = [("main", name) for (name,) in self.execute("SHOW TABLES").fetchall()]
        tables: list[str] = []
        for schema, name in rows:
            if schema and schema.lower() not in ("main", "temp"):
                tables.append(f"{schema}.{name}")
            else:
                tables.append(name)
        return tables

    def _fetch_all_table_columns(self, table_names: list[str]) -> dict[str, dict[str, str]]:
        if not table_names:
            return {}
        rows = self._query_duckdb_columns()
        if rows is None:
            return {
                name: cols
                for name in table_names
                if (cols := self._get_table_columns(name)) is not None
            }
        columns: dict[str, dict[str, str]] = {}
        known = {name.lower() for name in table_names}
        for _db, schema, table, column, data_type in rows:
            if schema and schema.lower() not in ("main", "temp", ""):
                qualified = f"{schema}.{table}"
            else:
                qualified = table
            if qualified.lower() not in known and table.lower() not in known:
                continue
            key = (
                qualified
                if qualified.lower() in known
                else next((n for n in table_names if n.lower() == table.lower()), qualified)
            )
            columns.setdefault(key, {})[column] = str(data_type).upper()
        return columns

    def _fetch_all_primary_keys(self, table_names: list[str]) -> dict[str, list[str]]:
        try:
            rows = self.execute(
                "SELECT table_name, constraint_column_names FROM duckdb_constraints() "
                "WHERE constraint_type = 'PRIMARY KEY'"
            ).fetchall()
        except duckdb.Error:
            return {}
        by_short: dict[str, list[str]] = {}
        for table_name, column_names in rows:
            for column in column_names:
                by_short.setdefault(table_name.lower(), []).append(column)
        keys: dict[str, list[str]] = {}
        for qualified in table_names:
            short = qualified.split(".")[-1].lower()
            if short in by_short:
                keys[qualified] = by_short[short]
        return keys

    def _table_row_count(self, table_name: str) -> int | None:
        try:
            row = self.execute(
                f"SELECT COUNT(*) FROM {self.quote_identifier(table_name)}"
            ).fetchone()
        except duckdb.Error:
            return None
        if row is None:
            return None
        return int(row[0])

    def _get_table_columns(self, table_name: str) -> dict[str, str] | None:
        try:
            rows = self.execute(f"DESCRIBE {self.quote_identifier(table_name)}").fetchall()
        except duckdb.Error:
            return None
        return {row[0]: str(row[1]).upper() for row in rows}

    def close(self) -> None:
        self._conn.close()
