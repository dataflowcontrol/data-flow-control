from __future__ import annotations

from .adapters.base import Adapter
from .adapters.duckdb import DuckDBAdapter
from .planner import Planner
from .policy import AggregatePolicy, PgnPolicy, Policy, Resolution


def strip_passant_comment(sql: str) -> str:
    if sql.startswith("-- passant:"):
        return "\n".join(sql.splitlines()[1:])
    return sql


def wrap(conn, dialect: str = "duckdb") -> Connection:
    if dialect != "duckdb":
        raise ValueError(f"Unsupported dialect: {dialect!r} (only 'duckdb' is supported)")
    return Connection(DuckDBAdapter(conn))


class Connection:
    """Policy-aware database wrapper around an adapter and Rust planner."""

    def __init__(self, adapter: Adapter, planner: Planner | None = None) -> None:
        self.adapter = adapter
        self.planner = planner or Planner(dialect=adapter.dialect)
        adapter.register_kill_function()

    @property
    def connection(self):
        duckdb_adapter = self.adapter
        if isinstance(duckdb_adapter, DuckDBAdapter):
            return duckdb_adapter.connection
        raise AttributeError("Underlying connection is only exposed for DuckDB adapters")

    def refresh_catalog(self) -> None:
        self.planner.sync_catalog(self.adapter.introspect_catalog())

    def register_policy(self, policy: Policy | AggregatePolicy | PgnPolicy) -> None:
        if isinstance(policy, Policy) and policy.on_fail == Resolution.KILL:
            if not self.adapter.capabilities.supports_kill:
                raise ValueError(
                    f"Resolution KILL is not supported for dialect {self.adapter.dialect!r}"
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
    ) -> str:
        return self.planner.rewrite(
            sql, use_partial_push=use_partial_push, collect_stats=collect_stats
        )

    def explain_rewrite(self, query: str) -> str:
        return self.planner.explain(query)

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

    def execute(self, query: str, *, use_partial_push: bool = False):
        rewritten = self.transform_query(query, use_partial_push=use_partial_push)
        executable = strip_passant_comment(rewritten)
        return self.adapter.execute(executable)

    def fetchall(self, query: str, *, use_partial_push: bool = False):
        return self.execute(query, use_partial_push=use_partial_push).fetchall()

    def fetchone(self, query: str, *, use_partial_push: bool = False):
        return self.execute(query, use_partial_push=use_partial_push).fetchone()

    def close(self) -> None:
        self.adapter.close()

    def __enter__(self) -> Connection:
        return self

    def __exit__(self, exc_type, exc_val, exc_tb) -> None:
        self.close()
