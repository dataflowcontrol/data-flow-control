from __future__ import annotations

from typing import Any

from .adapters.base import Adapter
from .adapters.duckdb import DuckDBAdapter
from .adapters.datafusion import DataFusionAdapter
from .adapters.registry import create_adapter, sniff_dialect
from .options import RewriteOptions
from .planner import Planner
from .policy import Policy, Resolution


def strip_passant_comment(sql: str) -> str:
    if sql.startswith("-- passant:"):
        return "\n".join(sql.splitlines()[1:])
    return sql


def dfc(conn: Any, dialect: str | None = None) -> Connection:
    resolved = dialect if dialect is not None else sniff_dialect(conn)
    adapter = create_adapter(conn, resolved)
    return Connection(adapter)


class Connection:
    """Policy-aware database wrapper around an adapter and Rust planner."""

    def __init__(self, adapter: Adapter, planner: Planner | None = None) -> None:
        self.adapter = adapter
        self.planner = planner or Planner(dialect=adapter.dialect)
        if adapter.capabilities.exception_udf:
            adapter.register_kill_function()

    @property
    def raw_connection(self):
        if isinstance(self.adapter, DuckDBAdapter):
            return self.adapter.connection
        if isinstance(self.adapter, DataFusionAdapter):
            return self.adapter.context
        if hasattr(self.adapter, "connection"):
            return self.adapter.connection
        raise AttributeError(
            f"Underlying connection is not exposed for dialect {self.adapter.dialect!r}"
        )

    def refresh_catalog(self) -> None:
        self.planner.sync_catalog(self.adapter.introspect_catalog())

    def register_resolution_function(
        self,
        name: str,
        func: Any,
        parameter_types: list[Any],
        return_type: Any,
    ) -> None:
        if not self.adapter.capabilities.tuple_udf:
            raise ValueError(
                f"Tuple UDF resolution is not supported for dialect {self.adapter.dialect!r}"
            )
        self.adapter.register_resolution_function(name, func, parameter_types, return_type)

    def register_relation_resolution_function(self, name: str, func: Any) -> None:
        if not self.adapter.capabilities.relation_udf:
            raise ValueError(
                f"Relation UDF resolution is not supported for dialect {self.adapter.dialect!r}"
            )
        self.adapter.register_relation_resolution_function(name, func)

    def register_policy(self, policy: Policy) -> None:
        if isinstance(policy, Policy):
            if policy.on_fail == Resolution.KILL:
                if not self.adapter.capabilities.exception_udf:
                    raise ValueError(
                        f"Resolution {policy.on_fail.value} is not supported for dialect "
                        f"{self.adapter.dialect!r}: missing capability exception_udf"
                    )
            elif policy.on_fail == Resolution.UDF:
                if not self.adapter.capabilities.tuple_udf:
                    raise ValueError(
                        f"Resolution {policy.on_fail_label} is not supported for dialect "
                        f"{self.adapter.dialect!r}: missing capability tuple_udf"
                    )
            elif policy.on_fail == Resolution.RELATION_UDF:
                if not self.adapter.capabilities.relation_udf:
                    raise ValueError(
                        f"Resolution {policy.on_fail_label} is not supported for dialect "
                        f"{self.adapter.dialect!r}: missing capability relation_udf"
                    )
        self.refresh_catalog()
        self.planner.register_policy(policy)

    def delete_policy(
        self,
        sources=None,
        sink=None,
        constraint: str = "",
        on_fail=None,
        description=None,
    ) -> bool:
        return self.planner.delete_policy(
            sources=sources,
            sink=sink,
            constraint=constraint,
            on_fail=on_fail,
            description=description,
        )

    def transform_query(self, sql: str, *, options: RewriteOptions | None = None) -> str:
        return self.planner.rewrite(sql, options=options)

    def explain(self, query: str) -> dict:
        return self.planner.explain_dict(query)

    def last_rewrite_stats(self):
        return self.planner.last_rewrite_stats()

    def last_statement_rewrite_summary(self):
        return self.planner.last_statement_rewrite_summary()

    def policies(self) -> list[Policy]:
        return self.planner.policies()

    def execute(self, query: str, *, params=None, options: RewriteOptions | None = None):
        rewritten = self.transform_query(query, options=options)
        executable = strip_passant_comment(rewritten)
        return self.adapter.execute(executable, params)

    def fetchall(self, query: str, *, params=None, options: RewriteOptions | None = None):
        result = self.execute(query, params=params, options=options)
        return result.fetchall()

    def fetchone(self, query: str, *, params=None, options: RewriteOptions | None = None):
        result = self.execute(query, params=params, options=options)
        return result.fetchone()

    def close(self) -> None:
        self.adapter.close()

    def __enter__(self) -> Connection:
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        self.close()
