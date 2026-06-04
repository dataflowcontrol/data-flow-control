from __future__ import annotations

import os
import re
import tempfile
from dataclasses import replace
from typing import Any

from .adapters.base import Adapter
from .adapters.duckdb import DuckDBAdapter
from .adapters.datafusion import DataFusionAdapter
from .adapters.registry import create_adapter, sniff_dialect
from .options import RewriteOptions, UiUpdateMode
from .planner import Planner
from .policy import Policy, Resolution
from .ui import (
    UiViolationHandler,
    build_ui_approval_event,
    build_ui_violation_event,
    merge_handler_row,
    write_ui_stream_row,
)


def strip_passant_comment(sql: str) -> str:
    if sql.startswith("-- passant:"):
        return "\n".join(sql.splitlines()[1:])
    return sql


_UI_STREAM_EXTENSION = "external"
_DDL_STATEMENT = re.compile(
    r"^\s*(CREATE|DROP|ALTER)\s+",
    re.IGNORECASE,
)


def _duckdb_extension_loaded(conn: Any, extension_name: str) -> bool:
    rows = conn.execute(
        "SELECT 1 FROM duckdb_extensions() WHERE loaded AND extension_name = ?",
        [extension_name],
    ).fetchall()
    return bool(rows)


def _ui_stream_union_statement_kind(sql: str) -> str | None:
    """Classify the outer statement for UI stream-union requirements."""
    import sqlglot
    from sqlglot import exp

    try:
        parsed = sqlglot.parse_one(sql.strip(), read="duckdb")
    except Exception:
        return None

    node = parsed.this if isinstance(parsed, exp.With) else parsed

    if isinstance(node, exp.Insert):
        return "insert"
    if isinstance(node, exp.Update):
        return "update"
    if isinstance(node, (exp.Select, exp.Union, exp.Intersect, exp.Except)):
        return "select"
    if isinstance(node, exp.Query):
        return _ui_stream_union_statement_kind(node.sql(dialect="duckdb"))
    return None


def _statement_needs_ui_stream_union(sql: str) -> bool:
    """True when rewritten SQL relies on extended_duckdb to union UI stream rows."""
    executable = strip_passant_comment(sql)
    if "address_violating_rows" not in executable:
        return False
    kind = _ui_stream_union_statement_kind(executable)
    return kind in ("insert", "select")


def dfc(conn: Any, dialect: str | None = None) -> Connection:
    resolved = dialect if dialect is not None else sniff_dialect(conn)
    adapter = create_adapter(conn, resolved)
    return Connection(adapter)


class Connection:
    """Policy-aware database wrapper around an adapter and Rust planner."""

    def __init__(self, adapter: Adapter, planner: Planner | None = None) -> None:
        self.adapter = adapter
        self.planner = planner or Planner(dialect=adapter.dialect)
        self._ui_handler: UiViolationHandler | None = None
        self._ui_stream_endpoint: str | None = None
        self._ui_update_mode: UiUpdateMode = UiUpdateMode.APPROVAL_ONLY
        self._ui_stream_extension_ready = False
        self._catalog_synced = False
        self._catalog_has_row_counts = False
        self._catalog_table_names: set[str] | None = None
        self._user_aggregate_functions: list[dict] = []
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

    def refresh_catalog(self, *, force: bool = False, include_row_counts: bool = False) -> None:
        if force or not self._catalog_synced:
            snapshot = self._introspect_catalog(include_row_counts=include_row_counts)
            self._apply_catalog_snapshot(snapshot, include_row_counts=include_row_counts)

    def _introspect_catalog(self, *, include_row_counts: bool = False) -> dict:
        if isinstance(self.adapter, DuckDBAdapter):
            snapshot = self.adapter.introspect_catalog(include_row_counts=include_row_counts)
        else:
            snapshot = self.adapter.introspect_catalog()
        snapshot = self._merge_aggregate_functions(snapshot)
        return snapshot

    def _merge_aggregate_functions(self, snapshot: dict) -> dict:
        introspected = list(snapshot.get("aggregate_functions") or [])
        if self._user_aggregate_functions:
            seen = {entry.get("name", "").lower() for entry in introspected}
            for entry in self._user_aggregate_functions:
                name = str(entry.get("name", "")).lower()
                if name and name not in seen:
                    introspected.append(entry)
                    seen.add(name)
        if introspected:
            snapshot["aggregate_functions"] = introspected
        return snapshot

    def refresh_aggregate_functions(self) -> None:
        """Re-sync aggregate metadata from the adapter (refreshes the catalog snapshot)."""
        snapshot = self._introspect_catalog(
            include_row_counts=self._catalog_has_row_counts,
        )
        self._apply_catalog_snapshot(
            snapshot,
            include_row_counts=self._catalog_has_row_counts,
        )

    def register_aggregate_function_name(
        self,
        name: str,
        *,
        schema: str | None = None,
        classification: str = "unknown_custom",
    ) -> None:
        """Declare a custom aggregate when introspection is unavailable or incomplete."""
        from .aggregate_introspection import normalize_aggregate_entry

        entry = normalize_aggregate_entry(
            name,
            schema=schema,
            classification=classification,
            source="user_declared",
        )
        self._user_aggregate_functions.append(entry)
        self.planner.register_aggregate_function_name(
            name,
            schema=schema,
            classification=classification,
        )

    def _apply_catalog_snapshot(self, snapshot: dict, *, include_row_counts: bool) -> None:
        self.planner.sync_catalog(snapshot)
        self._catalog_synced = True
        self._catalog_has_row_counts = include_row_counts
        self._catalog_table_names = set(snapshot.get("tables", {}).keys())

    def _policy_referenced_tables(self, policies: list[Policy]) -> set[str]:
        tables: set[str] = set()
        for policy in policies:
            tables.update(policy.sources or [])
            if policy.sink:
                tables.add(policy.sink)
            for dimension in policy.dimensions or []:
                tables.add(dimension)
            for alias, base in (policy.dimension_aliases or {}).items():
                tables.add(alias)
                tables.add(base)
        return tables

    def _catalog_covers_tables(self, table_names: set[str]) -> bool:
        if not self._catalog_table_names:
            return False
        known = {name.lower() for name in self._catalog_table_names}
        for table in table_names:
            normalized = table.lower()
            short = normalized.split(".")[-1]
            if normalized in known or short in known:
                continue
            if not any(
                entry == normalized or entry.endswith(f".{short}") or entry.split(".")[-1] == short
                for entry in known
            ):
                return False
        return True

    def _ensure_catalog_for_registration(self, policies: list[Policy]) -> None:
        referenced = self._policy_referenced_tables(policies)
        need_row_counts = any(p.dimensions for p in policies)
        if (
            self._catalog_synced
            and (not need_row_counts or self._catalog_has_row_counts)
            and self._catalog_covers_tables(referenced)
        ):
            return
        snapshot = self._introspect_catalog(include_row_counts=need_row_counts)
        self._apply_catalog_snapshot(snapshot, include_row_counts=need_row_counts)

    def _invalidate_catalog_cache(self) -> None:
        self._catalog_synced = False
        self._catalog_has_row_counts = False
        self._catalog_table_names = None

    @staticmethod
    def _statement_may_change_schema(sql: str) -> bool:
        stripped = strip_passant_comment(sql).strip()
        return bool(_DDL_STATEMENT.match(stripped))

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
        self._validate_policy_capabilities(policy)
        self._ensure_catalog_for_registration([policy])
        self.planner.register_policy(policy)

    def register_policies(self, policies: list[Policy]) -> None:
        for policy in policies:
            self._validate_policy_capabilities(policy)
        self._ensure_catalog_for_registration(policies)
        self.planner.register_policies(policies)

    def _validate_policy_capabilities(self, policy: Policy) -> None:
        if not isinstance(policy, Policy):
            return
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
        elif policy.on_fail == Resolution.UI:
            if not self.adapter.capabilities.ui_resolution:
                raise ValueError(
                    f"Resolution {policy.on_fail.value} is not supported for dialect "
                    f"{self.adapter.dialect!r}: missing capability ui_resolution"
                )
            if self._ui_handler is None:
                raise ValueError(
                    "Resolution UI requires configure_ui_resolution() before register_policy()"
                )

    def configure_ui_resolution(
        self,
        handler: UiViolationHandler,
        *,
        stream_endpoint: str | None = None,
        extension_path: str | None = None,
        polling_ms: int = 100,
        update_mode: UiUpdateMode | str = UiUpdateMode.APPROVAL_ONLY,
        max_wait_seconds: int = 60,
    ) -> None:
        if not self.adapter.capabilities.ui_resolution:
            raise ValueError(f"UI resolution is not supported for dialect {self.adapter.dialect!r}")
        if not isinstance(self.adapter, DuckDBAdapter):
            raise ValueError("UI resolution is only supported for DuckDB connections")

        if stream_endpoint is None:
            fd, stream_endpoint = tempfile.mkstemp(suffix=".tsv", prefix="passant-ui-")
            os.close(fd)
        else:
            parent = os.path.dirname(stream_endpoint)
            if parent:
                os.makedirs(parent, exist_ok=True)
        self._clear_ui_stream_file(stream_endpoint)

        conn = self.adapter.connection
        if isinstance(update_mode, str):
            update_mode = UiUpdateMode(update_mode)
        self._ui_update_mode = update_mode

        extension_ready = False
        if extension_path is not None:
            conn.execute(f"LOAD {extension_path!r}")
            extension_ready = True
        elif _duckdb_extension_loaded(conn, _UI_STREAM_EXTENSION):
            extension_ready = True

        if extension_ready:
            conn.execute(f"PRAGMA external_stream_endpoint({stream_endpoint!r})")
            conn.execute(f"PRAGMA external_polling_ms({int(polling_ms)})")
            conn.execute(f"PRAGMA external_max_wait_seconds({int(max_wait_seconds)})")
        self._ui_stream_extension_ready = extension_ready

        self._ui_handler = handler
        self._ui_stream_endpoint = stream_endpoint

        def address_violating_rows(*args: Any) -> bool:
            event = build_ui_violation_event(args)
            corrected = handler(event)
            if corrected is None:
                return False
            endpoint = event.stream_endpoint or stream_endpoint
            row_values = merge_handler_row(event, corrected)
            write_ui_stream_row(endpoint, event.column_names, row_values)
            return False

        conn.create_function(
            "address_violating_rows",
            address_violating_rows,
            null_handling="special",
            return_type="BOOLEAN",
            side_effects=True,
        )

        def passant_ui_approve(*args: Any) -> bool:
            event = build_ui_approval_event(args)
            corrected = handler(event)
            return corrected is not None

        conn.create_function(
            "passant_ui_approve",
            passant_ui_approve,
            null_handling="special",
            return_type="BOOLEAN",
            side_effects=True,
        )

    def reset_ui_stream(self) -> None:
        if self._ui_stream_endpoint is not None:
            self._clear_ui_stream_file(self._ui_stream_endpoint)

    def _clear_ui_stream_file(self, path: str) -> None:
        with open(path, "w", encoding="utf-8"):
            pass

    def _effective_rewrite_options(self, options: RewriteOptions | None) -> RewriteOptions:
        opts = options or RewriteOptions()
        if self._ui_stream_endpoint is not None and opts.ui_stream_endpoint is None:
            opts = replace(opts, ui_stream_endpoint=self._ui_stream_endpoint)
        if self._ui_handler is not None:
            opts = replace(opts, ui_update_mode=self._ui_update_mode)
        return opts

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
        return self.planner.rewrite(sql, options=self._effective_rewrite_options(options))

    def explain(self, query: str) -> dict:
        return self.planner.explain_dict(query)

    def last_rewrite_stats(self):
        return self.planner.last_rewrite_stats()

    def last_statement_rewrite_summary(self):
        return self.planner.last_statement_rewrite_summary()

    def policies(self) -> list[Policy]:
        return self.planner.policies()

    def _ensure_ui_stream_extension_for_execute(self, executable: str) -> None:
        if not _statement_needs_ui_stream_union(executable):
            return
        if self._ui_stream_extension_ready:
            return
        if not isinstance(self.adapter, DuckDBAdapter):
            return
        conn = self.adapter.connection
        if _duckdb_extension_loaded(conn, _UI_STREAM_EXTENSION):
            self._ui_stream_extension_ready = True
            return
        raise ValueError(
            "UI resolution for INSERT and SELECT requires the extended_duckdb "
            f"'{_UI_STREAM_EXTENSION}' extension so corrected rows can be unioned into "
            "results. Call configure_ui_resolution(..., extension_path=...) or LOAD the "
            "extension before execute()."
        )

    def execute(self, query: str, *, params=None, options: RewriteOptions | None = None):
        if self._ui_stream_endpoint is not None:
            self.reset_ui_stream()
        if self._statement_may_change_schema(query):
            self._invalidate_catalog_cache()
            return self.adapter.execute(query, params)
        rewritten = self.transform_query(query, options=options)
        executable = strip_passant_comment(rewritten)
        self._ensure_ui_stream_extension_for_execute(executable)
        result = self.adapter.execute(executable, params)
        if self._statement_may_change_schema(query):
            self._invalidate_catalog_cache()
        followup = self.planner.last_ui_followup_sql()
        if followup:
            self.adapter.execute(followup, params)
        return result

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
