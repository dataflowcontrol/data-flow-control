from __future__ import annotations

from typing import Any

from ..dialect import Dialect
from .base import Adapter
from .duckdb import DuckDBAdapter


def _sniff_failure_message() -> str:
    supported = ", ".join(Dialect.supported_names())
    return (
        "Could not infer database dialect from the connection. "
        f"Set dialect explicitly to one of: {supported}"
    )


def sniff_dialect(conn: Any) -> str:
    """Infer dialect from a driver connection or Passant adapter via type inspection."""
    adapter_dialect = getattr(conn, "dialect", None)
    if isinstance(adapter_dialect, str) and Dialect.contains(adapter_dialect):
        return adapter_dialect

    import duckdb

    if isinstance(conn, duckdb.DuckDBPyConnection):
        return "duckdb"

    import sqlite3

    if isinstance(conn, sqlite3.Connection):
        return "sqlite"

    conn_type = type(conn)
    module = conn_type.__module__
    type_name = conn_type.__name__

    if "datafusion" in module and type_name == "SessionContext":
        return "datafusion"

    if "clickhouse_connect" in module:
        return "clickhouse"

    if module.startswith("psycopg"):
        return "postgres"

    raise ValueError(_sniff_failure_message())


def create_adapter(conn: Any, dialect: str) -> Adapter:
    parsed = Dialect.parse(dialect)
    if parsed is Dialect.DUCKDB:
        return DuckDBAdapter(conn)
    if parsed is Dialect.SQLITE:
        from .sqlite import SQLiteAdapter

        return SQLiteAdapter(conn)
    if parsed is Dialect.POSTGRES:
        from .postgres import PostgresAdapter

        return PostgresAdapter(conn)
    if parsed is Dialect.CLICKHOUSE:
        from .clickhouse import ClickHouseAdapter

        return ClickHouseAdapter(conn)
    if parsed is Dialect.DATAFUSION:
        from .datafusion import DataFusionAdapter

        return DataFusionAdapter(conn)
    if parsed is Dialect.UMBRA:
        from .umbra import UmbraAdapter

        return UmbraAdapter(conn)
    raise NotImplementedError(f"No adapter factory for dialect {parsed.value!r}")
