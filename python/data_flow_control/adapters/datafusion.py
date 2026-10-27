from __future__ import annotations

from typing import Any

from ..aggregate_introspection import introspect_datafusion_aggregates
from ..catalog import build_catalog_snapshot
from ._retry import register_retry
from .base import Capabilities
from .kill import python_kill


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
    capabilities = Capabilities(exception_udf=True, retry_resolution=True)

    def __init__(self, context) -> None:
        self._ctx = context
        self._retry_state = None

    @property
    def context(self):
        return self._ctx

    def execute(self, sql: str, params: Any = None):
        if params is not None:
            raise ValueError("DataFusion adapter does not support query parameters")
        if self._retry_state is None:
            return _DataFusionCursor(self._ctx.sql(sql).collect())
        with self._retry_state.execution():
            return _DataFusionCursor(self._ctx.sql(sql).collect())

    def register_retry_function(self) -> None:
        import pyarrow as pa
        from datafusion import udf

        def install(state):
            def retry_many(policy_ids):
                # Native lowering calls this only for a failing relation guard.
                for value in policy_ids.to_pylist():
                    state.signal(value)
                return pa.array([], type=pa.bool_())

            self._ctx.register_udf(
                udf(retry_many, [pa.string()], pa.bool_(), "volatile", name="passant_retry_many")
            )

        self._retry_state = register_retry(self._ctx, install)

    def quote_identifier(self, name: str) -> str:
        from .duckdb import quote_sql_identifier

        return quote_sql_identifier(name)

    def register_kill_function(self) -> None:
        import pyarrow as pa
        from datafusion import udf

        def _kill() -> pa.Array:
            python_kill()
            return pa.array([True], type=pa.bool_())

        for name in ("kill", "passant_kill"):
            self._ctx.register_udf(udf(_kill, [], pa.bool_(), "volatile", name=name))

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
            aggregate_functions=self.introspect_aggregate_functions(),
        )

    def introspect_aggregate_functions(self) -> list[dict]:
        return introspect_datafusion_aggregates(self._ctx)

    def close(self) -> None:
        return
