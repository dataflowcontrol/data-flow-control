from __future__ import annotations

from threading import Lock
from typing import Any
from weakref import WeakKeyDictionary

import duckdb

from ..aggregate_introspection import introspect_duckdb_aggregates
from ..catalog import build_catalog_snapshot
from ..errors import _NativeRetrySignal
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


_kill_registrations: WeakKeyDictionary = WeakKeyDictionary()
_retry_registrations: WeakKeyDictionary = WeakKeyDictionary()
_retry_registration_lock = Lock()


class DuckDBAdapter:
    dialect = "duckdb"
    capabilities = Capabilities(
        exception_udf=True,
        tuple_udf=True,
        relation_udf=True,
        ui_resolution=True,
        retry_resolution=True,
    )

    def __init__(self, conn: duckdb.DuckDBPyConnection) -> None:
        self._conn = conn

    @property
    def connection(self) -> duckdb.DuckDBPyConnection:
        return self._conn

    def execute(self, sql: str, params: Any = None):
        state = _retry_registrations.get(self._conn)
        if state is not None:
            if not state["lock"].acquire(blocking=False):
                raise RuntimeError("Overlapping executions on a RETRY connection are not supported")
            state["failure"] = None
            state["active"] = True
        try:
            if params is None:
                return self._conn.execute(sql)
            return self._conn.execute(sql, params)
        except duckdb.Error as exc:
            failure = state["failure"] if state is not None else None
            if failure is not None:
                raise _NativeRetrySignal(failure) from exc
            raise
        finally:
            if state is not None:
                state["failure"] = None
                state["active"] = False
                state["lock"].release()

    def quote_identifier(self, name: str) -> str:
        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        if self._conn in _kill_registrations:
            return
        created = []
        try:
            for name in ("passant_kill", "kill"):
                self._conn.create_function(
                    name,
                    python_kill,
                    [],
                    "BOOLEAN",
                    side_effects=True,
                )
                created.append(name)
        except duckdb.Error as exc:
            for registered in reversed(created):
                self._conn.remove_function(registered)
            raise RuntimeError(
                f"Cannot register DFC KILL function {name!r}. "
                "The connection must allow UDF registration and must not already "
                "define the reserved names 'kill' or 'passant_kill'."
            ) from exc
        _kill_registrations[self._conn] = True

    def register_retry_function(self) -> None:
        # All adapters wrapping one raw connection share execution-local state.
        # A callback sets evidence before DuckDB wraps its exception; ordinary
        # backend errors are never classified from their text.
        with _retry_registration_lock:
            if self._conn in _retry_registrations:
                return
            state = {"failure": None, "active": False, "lock": Lock()}

            def retry_many(policy_ids: str) -> bool:
                if not state["active"]:
                    raise RuntimeError("Native RETRY must execute through the DFC adapter")
                parts = policy_ids.split(",") if isinstance(policy_ids, str) else []
                if not parts or any(
                    not part or not part.isascii() or not part.isdigit() for part in parts
                ):
                    raise RuntimeError("Invalid native RETRY policy identity list")
                failure = tuple((index, None) for index in sorted({int(part) for part in parts}))
                state["failure"] = failure
                raise _NativeRetrySignal(failure)

            self._conn.create_function(
                "passant_retry_many",
                retry_many,
                ["VARCHAR"],
                "BOOLEAN",
                side_effects=True,
                null_handling="special",
            )
            _retry_registrations[self._conn] = state

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
            side_effects=True,
        )

    def register_relation_resolution_function(self, name: str, func: Any) -> None:
        """Register a scalar UDF invoked with bool_or(violation) over the relation input."""
        self._conn.create_function(name, func, ["BOOLEAN"], "BOOLEAN", side_effects=True)

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
