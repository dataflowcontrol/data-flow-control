from __future__ import annotations

from typing import Any

from .adapters.base import Adapter
from .adapters.duckdb import DuckDBAdapter
from .adapters.datafusion import DataFusionAdapter
from .adapters.registry import connect as open_connection
from .adapters.registry import create_adapter
from .options import RewriteOptions
from .planner import Planner
from .policy import AggregatePolicy, PgnPolicy, Policy, Resolution


def strip_passant_comment(sql: str) -> str:
    if sql.startswith("-- passant:"):
        return "\n".join(sql.splitlines()[1:])
    return sql


def wrap(conn: Any, dialect: str = "duckdb") -> Connection:
    adapter = create_adapter(conn, dialect)
    return Connection(adapter)


def connect(url: str, *, dialect: str | None = None) -> Connection:
    conn, resolved = open_connection(url, dialect=dialect)
    return wrap(conn, dialect=resolved)


class Connection:
    """Policy-aware database wrapper around an adapter and Rust planner."""

    def __init__(self, adapter: Adapter, planner: Planner | None = None) -> None:
        self.adapter = adapter
        self.planner = planner or Planner(dialect=adapter.dialect)
        if adapter.capabilities.exception_udf:
            adapter.register_kill_function()

    @property
    def connection(self):
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

    def register_policy(self, policy: Policy | AggregatePolicy | PgnPolicy) -> None:
        if isinstance(policy, Policy) and policy.on_fail == Resolution.KILL:
            if not self.adapter.capabilities.exception_udf:
                raise ValueError(
                    f"Resolution KILL requires exception UDF support; "
                    f"dialect {self.adapter.dialect!r} does not provide it"
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

    def transform_query(
        self,
        sql: str,
        *,
        use_partial_push: bool = False,
        collect_stats: bool = False,
        options: RewriteOptions | None = None,
    ) -> str:
        opts = options or RewriteOptions(
            use_partial_push=use_partial_push,
            collect_stats=collect_stats,
        )
        return self.planner.rewrite(
            sql,
            options=opts,
        )

    def explain_rewrite(self, query: str) -> dict:
        return self.planner.explain_dict(query)

    def last_rewrite_stats(self):
        return self.planner.last_rewrite_stats()

    def last_statement_rewrite_summary(self):
        return self.planner.last_statement_rewrite_summary()

    def policies(self) -> list[Policy]:
        return self.planner.policies()

    def aggregate_policies(self) -> list[AggregatePolicy]:
        return self.planner.aggregate_policies()

    def pgn_policies(self) -> list[PgnPolicy]:
        return self.planner.pgn_policies()

    def execute(
        self,
        query: str,
        *,
        params=None,
        use_partial_push: bool = False,
        collect_stats: bool = False,
    ):
        rewritten = self.transform_query(
            query,
            use_partial_push=use_partial_push,
            collect_stats=collect_stats,
        )
        executable = strip_passant_comment(rewritten)
        return self.adapter.execute(executable, params)

    def fetchall(
        self,
        query: str,
        *,
        params=None,
        use_partial_push: bool = False,
        collect_stats: bool = False,
    ):
        result = self.execute(
            query,
            params=params,
            use_partial_push=use_partial_push,
            collect_stats=collect_stats,
        )
        return result.fetchall()

    def fetchone(
        self,
        query: str,
        *,
        params=None,
        use_partial_push: bool = False,
        collect_stats: bool = False,
    ):
        result = self.execute(
            query,
            params=params,
            use_partial_push=use_partial_push,
            collect_stats=collect_stats,
        )
        return result.fetchone()

    def close(self) -> None:
        self.adapter.close()

    def __enter__(self) -> Connection:
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        self.close()
