"""Host integration for Rust-planned SQL provenance of external tool outputs.

SQL parsing, correspondence, policy interpretation and enforcement planning live
in passant-core. This module converts host arguments and executes native plans
inside a DuckDB transaction; it never executes the external tool itself.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

from . import _passant
from .adapters.duckdb import DuckDBAdapter
from .connection import Connection
from .errors import RetryFailure, RetryViolation, _NativeRetrySignal


@dataclass(frozen=True)
class ToolOutput:
    """An actual output row and its out-of-band modeled occurrence key."""

    key: tuple[Any, ...]
    values: Mapping[str, Any]


@dataclass(frozen=True)
class ToolProvenance:
    """A trusted SQL model compiled and validated by the Rust core.

    Keys identify modeled occurrences, not original contributors. An empty key
    describes a singleton output. Keys do not change business output schemas.
    """

    sql: str
    key_columns: tuple[str, ...] = ()
    _native: Any = field(init=False, repr=False, compare=False)

    def __post_init__(self) -> None:
        native = _passant.ToolProvenanceModel(self.sql, list(self.key_columns))
        object.__setattr__(self, "_native", native)
        object.__setattr__(self, "sql", native.sql)
        object.__setattr__(self, "key_columns", tuple(native.key_columns))

    def bind_intermediates(
        self, result: Any, outputs: Sequence[ToolOutput]
    ) -> tuple[tuple[Any, ...], ...]:
        """Align opaque whole-state tuples using the native occurrence matcher.

        Pass a fresh automatic-propagation model result. Binding neither stores
        observations nor authorizes effects or sink publication.
        """
        states = getattr(result, "intermediates", None)
        description = result.description
        columns = None if description is None else [column[0] for column in description]
        indices = self._native.bind_indices(
            columns,
            result.fetchall(),
            None if states is None else len(states),
            [tuple(output.key) for output in outputs],
        )
        return tuple(states[index] for index in indices)

    def insert_outputs(
        self,
        connection: Connection,
        sink: str,
        outputs: Sequence[ToolOutput],
        *,
        params: Sequence[Any] = (),
    ) -> int:
        """Execute the native publication plan in an exclusive DuckDB transaction.

        Actual execution is external. The model must remain deterministic within
        this snapshot. Do not call inside an existing transaction or share the
        connection concurrently.
        """
        if getattr(connection, "_intermediate_execution", None) is not None:
            raise ValueError(
                "insert_outputs uses the legacy SQL rewriter; in automatic intermediate mode "
                "use bind_intermediates and the trusted store_intermediate_outputs bridge"
            )
        if not isinstance(connection.adapter, DuckDBAdapter):
            raise NotImplementedError("Tool provenance publication currently requires DuckDB")
        outputs = tuple(outputs)
        params = tuple(params)
        raw = connection.raw_connection
        # BEGIN stays outside try so a rejected nested transaction does not
        # cause this host wrapper to roll back a transaction owned by its caller.
        raw.execute("BEGIN TRANSACTION")
        try:
            connection.refresh_catalog(force=True, include_row_counts=True)
            plan = self._native.plan(
                connection.planner.inner,
                sink,
                [tuple(output.key) for output in outputs],
                [list(output.values) for output in outputs],
                len(params),
            )
            bindings = list(params)
            for output in outputs:
                bindings.extend(output.key)
                bindings.extend(output.values[column] for column in plan.output_columns)
            status = raw.execute(plan.validation_sql, bindings).fetchone()[0]
            plan.check_validation(status)
            if plan.sink_kill_preflight_sql is not None:
                preflight_bindings = [
                    bindings[index] for index in plan.sink_preflight_binding_indices
                ]
                connection.adapter.execute(
                    plan.sink_kill_preflight_sql, preflight_bindings
                ).fetchall()
            inserted = 0
            if plan.publication_sql is not None:
                inserted = int(
                    _execute_native_publication(connection, plan.publication_sql, bindings)[0]
                )
            raw.execute("COMMIT")
            return inserted
        except BaseException:
            raw.execute("ROLLBACK")
            raise


def _execute_native_publication(connection: Connection, sql: str, bindings: list[Any]):
    """Translate adapter callback evidence into the existing host exception API."""
    try:
        return connection.adapter.execute(sql, bindings).fetchone()
    except _NativeRetrySignal as exc:
        failures = []
        for index, _ in exc.failures:
            policy = connection.planner.policy_by_index(index)
            if policy is None:
                raise RuntimeError(
                    "Native tool publication returned an unknown RETRY policy"
                ) from exc
            failures.append(RetryFailure(index, policy.retry_message, policy))
        raise RetryViolation(failures) from exc
