from __future__ import annotations

from typing import Any

from ..catalog import build_catalog_snapshot
from .base import Capabilities


class _DataFusionCursor:
    def __init__(self, batches) -> None:
        self._batches = batches

    def fetchall(self) -> list[tuple]:
        rows: list[tuple] = []
        for batch in self._batches:
            if batch.num_rows == 0:
                continue
            columns = batch.column_names
            for row_index in range(batch.num_rows):
                rows.append(tuple(batch[column][row_index].as_py() for column in columns))
        return rows

    def fetchone(self):
        rows = self.fetchall()
        return rows[0] if rows else None


class DataFusionAdapter:
    dialect = "datafusion"
    capabilities = Capabilities(exception_udf=False)

    def __init__(self, context) -> None:
        self._ctx = context

    @property
    def context(self):
        return self._ctx

    def execute(self, sql: str, params: Any = None):
        if params is not None:
            raise ValueError("DataFusion adapter does not support query parameters")
        return _DataFusionCursor(self._ctx.sql(sql).collect())

    def quote_identifier(self, name: str) -> str:
        from .duckdb import quote_sql_identifier

        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        return

    def introspect_catalog(self) -> dict:
        tables: dict[str, dict] = {}
        schema = self._ctx.catalog().schema("public")
        for table_name in sorted(schema.table_names()):
            df_schema = self._ctx.table(table_name).schema()
            column_types = {field.name: str(field.type).upper() for field in df_schema}
            tables[table_name] = {
                "columns": list(column_types.keys()),
                "types": column_types,
            }
        return build_catalog_snapshot(
            dialect=self.dialect,
            tables=tables,
            default_schema="public",
            search_path=["public"],
        )

    def close(self) -> None:
        return
