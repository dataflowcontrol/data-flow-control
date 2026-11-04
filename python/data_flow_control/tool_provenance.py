"""Trusted SQL models for publishing externally computed tool outputs.

The model is relational SQL, not an implementation of the external tool. See
``developer-docs/tool-provenance.md`` for the trust and snapshot contract.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from typing import Any

import sqlglot
from sqlglot import exp
from sqlglot.optimizer.qualify import qualify
from sqlglot.optimizer.merge_subqueries import merge_subqueries
from sqlglot.tokens import TokenType

from .adapters.duckdb import DuckDBAdapter
from .connection import Connection
from .errors import RetryFailure, RetryViolation, _NativeRetrySignal
from .planner import Planner
from .policy import Resolution


def _quote(name: str) -> str:
    if not isinstance(name, str) or not name:
        raise ValueError("SQL identifiers must be nonempty strings")
    return '"' + name.replace('"', '""') + '"'


@dataclass(frozen=True)
class ToolOutput:
    """An actual output row and its out-of-band modeled occurrence key."""

    key: tuple[Any, ...]
    values: Mapping[str, Any]


@dataclass(frozen=True)
class ToolProvenance:
    """A trusted, deterministic SELECT over the tool's complete input relations.

    ``key_columns`` identify modeled output occurrences, not original source
    contributors. Empty key columns describe a singleton output. Keys need not
    be stored in the business output schema.
    """

    sql: str
    key_columns: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        statements = sqlglot.parse(self.sql, read="duckdb")
        if len(statements) != 1 or not isinstance(statements[0], exp.Query):
            raise ValueError("Tool provenance requires one SELECT query")
        query = statements[0]
        if any(isinstance(node, (exp.DML, exp.DDL)) for node in query.walk()):
            raise ValueError("Tool provenance requires a read-only query")
        if any(
            isinstance(node, exp.Parameter) or (isinstance(node, exp.Placeholder) and node.this)
            for node in query.walk()
        ):
            raise ValueError("Tool models support positional ? parameters only")
        # This is an early diagnostic, not a sandbox or a proof of determinism.
        if any(isinstance(node, (exp.Rand, exp.Uuid)) for node in query.walk()):
            raise ValueError("Tool provenance must be deterministic; bind observed values instead")
        if len(set(self.key_columns)) != len(self.key_columns):
            raise ValueError("Modeled key columns must be distinct")
        for column in self.key_columns:
            _quote(column)
        object.__setattr__(self, "sql", query.sql(dialect="duckdb"))
        object.__setattr__(self, "key_columns", tuple(self.key_columns))

    def bind_intermediates(
        self, result: Any, outputs: Sequence[ToolOutput]
    ) -> tuple[tuple[Any, ...], ...]:
        """Match a fresh automatic-propagation model result to actual outputs.

        Execute this declaration with ``connection.execute(model.sql)`` first.
        This trusted bridge only binds states; it neither stores output rows nor
        evaluates policies against actual payloads. The caller may record tool
        observations with ``connection.store_intermediate_outputs`` and enforce
        subsequent sink operations through normal automatic execution.
        """
        states = getattr(result, "intermediates", None)
        if states is None or result.description is None:
            raise ValueError("Model result must carry automatic intermediate states")
        columns = [column[0].casefold() for column in result.description]
        positions = []
        for key in self.key_columns:
            if columns.count(key.casefold()) != 1:
                raise ValueError("Modeled key columns must identify unique result columns")
            positions.append(columns.index(key.casefold()))
        rows = result.fetchall()
        if len(rows) != len(states):
            raise ValueError("Model result must be fresh and have one intermediate tuple per row")
        modeled_keys = [tuple(row[index] for index in positions) for row in rows]
        actual_keys = [tuple(output.key) for output in outputs]
        if any(len(key) != len(self.key_columns) for key in actual_keys):
            raise ValueError("Output key arity does not match key_columns")
        _unique_keys(modeled_keys, "Modeled")
        _unique_keys(actual_keys, "Actual")
        if set(modeled_keys) != set(actual_keys):
            raise ValueError("Actual outputs do not match all modeled output keys")
        by_key = dict(zip(modeled_keys, states, strict=True))
        return tuple(by_key[key] for key in actual_keys)

    def insert_outputs(
        self,
        connection: Connection,
        sink: str,
        outputs: Sequence[ToolOutput],
        *,
        params: Sequence[Any] = (),
    ) -> int:
        """Validate correspondence and publish successful outputs through DFC.

        Owns a DuckDB transaction; call outside an existing transaction and do
        not share the connection concurrently. Tool execution is the caller's
        responsibility and is never retried here. The model must be stable
        across validation and insertion within this transaction.
        """
        if getattr(connection, "_intermediate_execution", None) is not None:
            raise ValueError(
                "insert_outputs uses the legacy SQL rewriter; in automatic intermediate mode "
                "use bind_intermediates and the trusted store_intermediate_outputs bridge"
            )
        if not isinstance(connection.adapter, DuckDBAdapter):
            raise NotImplementedError("Tool provenance publication currently requires DuckDB")
        target = _quote(sink)
        outputs = tuple(outputs)
        keys = [tuple(output.key) for output in outputs]
        if any(len(key) != len(self.key_columns) for key in keys):
            raise ValueError("Output key arity does not match key_columns")
        _unique_keys(keys, "Actual")
        columns = tuple(outputs[0].values) if outputs else ()
        if outputs and not columns:
            raise ValueError("Actual output rows must have at least one column")
        for column in columns:
            _quote(column)
        if any(set(output.values) != set(columns) for output in outputs):
            raise ValueError("Actual output rows must have identical column names")

        raw = connection.raw_connection
        # BEGIN is deliberately outside try: a nested transaction error must
        # not roll back a caller's existing transaction.
        raw.execute("BEGIN TRANSACTION")
        try:
            whole_output_remove = _validate_publication_shape(connection, self.sql)
            model_sql = _expand_stars(raw, self.sql)
            key_projection = ", ".join(map(_quote, self.key_columns)) or "1"
            modeled = raw.execute(
                f"SELECT {key_projection} FROM ({model_sql}) AS model", list(params)
            ).fetchall()
            modeled_keys = [tuple(row) if self.key_columns else () for row in modeled]
            _unique_keys(modeled_keys, "Modeled")
            if set(modeled_keys) != set(keys):
                raise ValueError("Actual outputs do not match all modeled output keys")
            if not outputs:
                raw.execute("COMMIT")
                return 0

            # Preserve the entire model as an ordinary relational input to the
            # INSERT. Never reconstruct or divide a row's original contributors.
            key_names = [f"k{i}" for i in range(len(self.key_columns))]
            value_names = [f"v{i}" for i in range(len(columns))]
            aliases = key_names + value_names
            placeholders = "(" + ", ".join("?" for _ in aliases) + ")"
            actual = (
                "(VALUES "
                + ", ".join(placeholders for _ in outputs)
                + ") AS actual("
                + ", ".join(aliases)
                + ")"
            )
            predicate = (
                " AND ".join(
                    f"model.{_quote(column)} IS NOT DISTINCT FROM actual.{name}"
                    for column, name in zip(self.key_columns, key_names, strict=True)
                )
                or "TRUE"
            )
            source = f"({model_sql}) AS model JOIN {actual} ON {predicate}"
            bindings = list(params)
            for output in outputs:
                bindings.extend(output.key)
                bindings.extend(output.values[column] for column in columns)
            # SQL comparison may have different equality semantics from Python
            # (collations/coercions). Require a bijection in SQL too.
            matches = raw.execute(f"SELECT COUNT(*) FROM {source}", bindings).fetchone()[0]
            if matches != len(outputs):
                raise ValueError("Output keys are ambiguous under SQL equality")
            projection = ", ".join(f"actual.{name}" for name in value_names)
            sql = (
                f"INSERT INTO {target} ({', '.join(map(_quote, columns))}) "
                f"SELECT {projection} FROM {source}"
            )
            # Keep actual output values in scope for source/sink comparisons
            # where SQLGlot can safely merge the model's derived table. Number
            # parameters first: merging can move WHERE after the VALUES join.
            query = sqlglot.parse_one(_number_parameters(sql), read="duckdb")
            # SQLGlot may discard unused window or unknown-function projections
            # when merging. They can carry dependencies or change cardinality
            # even though actual payloads do not reference their result values.
            if query.find(exp.Window, exp.Anonymous) is None:
                query = merge_subqueries(query)
            # The Rust parser accepts CROSS JOIN LATERAL but not comma LATERAL.
            # Normalize only unqualified lateral joins; preserve outer/ON joins.
            for join in query.find_all(exp.Join):
                if isinstance(join.this, exp.Lateral) and not any(
                    join.args.get(part) for part in ("on", "using", "side", "kind", "method")
                ):
                    join.set("kind", "CROSS")
            sql = _normalize_aggregate_aliases(query.sql(dialect="duckdb"))
            if whole_output_remove:
                sink_retry = None
                try:
                    _preflight_sink_aborts(connection, sink, columns, outputs)
                except RetryViolation as exc:
                    sink_retry = exc
                try:
                    result = _execute_whole_output_remove(connection, sql, bindings)
                except RetryViolation as exc:
                    failures = [*exc.failures, *(sink_retry.failures if sink_retry else ())]
                    raise RetryViolation(failures) from exc
                if sink_retry is not None:
                    raise sink_retry
            else:
                result = connection.execute(sql, params=bindings).fetchone()
            raw.execute("COMMIT")
            return int(result[0])
        except BaseException:
            raw.execute("ROLLBACK")
            raise


def _unique_keys(keys: list[tuple[Any, ...]], kind: str) -> None:
    try:
        unique = set(keys)
    except TypeError as exc:
        raise ValueError(f"{kind} output keys must contain hashable scalar values") from exc
    if len(unique) != len(keys):
        raise ValueError(f"{kind} output keys must be unique; distinguish duplicate occurrences")
    if any(value is None or value != value for key in keys for value in key):
        raise ValueError(f"{kind} output keys must not contain NULL or NaN")


def _expand_stars(raw: Any, sql: str) -> str:
    """Expand wildcards for the existing tuple-resolution rewriter.

    Qualify with a full catalog for unambiguous schema lookup, then restore the
    author's table qualification: adding catalog prefixes changes DFC source
    names. This is normalization, not provenance transformation.
    """
    query = sqlglot.parse_one(sql, read="duckdb")
    if not any(
        item.is_star for select in query.find_all(exp.Select) for item in select.expressions
    ):
        return sql
    schema: dict[str, Any] = {}
    for catalog, namespace, table, column, dtype in raw.execute(
        "SELECT table_catalog, table_schema, table_name, column_name, data_type "
        "FROM information_schema.columns ORDER BY ordinal_position"
    ).fetchall():
        schema.setdefault(catalog, {}).setdefault(namespace, {}).setdefault(table, {})[column] = (
            dtype
        )
    catalog, namespace = raw.execute("SELECT current_database(), current_schema()").fetchone()
    original = [
        (table, table.args.get("db"), table.args.get("catalog"))
        for table in query.find_all(exp.Table)
    ]
    qualify(
        query,
        dialect="duckdb",
        schema=schema,
        db=namespace,
        catalog=catalog,
        validate_qualify_columns=False,
    )
    for table, db, catalog in original:
        table.set("db", db)
        table.set("catalog", catalog)
    return query.sql(dialect="duckdb")


def _number_parameters(sql: str) -> str:
    tokens = [
        token
        for token in sqlglot.Dialect.get_or_raise("duckdb").tokenize(sql)
        if token.token_type == TokenType.PLACEHOLDER
    ]
    for index, token in reversed(list(enumerate(tokens, start=1))):
        sql = sql[: token.start] + f"${index}" + sql[token.end + 1 :]
    return sql


def _preflight_sink_aborts(
    connection: Connection, sink: str, columns: tuple[str, ...], outputs: Sequence[ToolOutput]
) -> None:
    """Check original actual candidates before source REMOVE can hide an abort.

    Ask the ordinary INSERT rewriter to establish sink scope, then execute only
    its rewritten source query. No preflight row is written to the sink.
    """
    policies = [
        policy
        for policy in connection.policies()
        if not policy.sources and policy.on_fail in (Resolution.KILL, Resolution.RETRY)
    ]
    if not policies:
        return
    planner = Planner(dialect=connection.planner.dialect)
    planner.sync_catalog(connection._introspect_catalog(include_row_counts=True))
    planner.register_policies(policies)
    candidates = " UNION ALL ".join("SELECT " + ", ".join("?" for _ in columns) for _ in outputs)
    sql = f"INSERT INTO {_quote(sink)} ({', '.join(map(_quote, columns))}) {candidates}"
    rewritten = sqlglot.parse_one(planner.rewrite(sql), read="duckdb")
    if not isinstance(rewritten, exp.Insert) or rewritten.expression is None:
        raise ValueError("Tool sink preflight requires a rewritten INSERT source")
    bindings = [output.values[column] for output in outputs for column in columns]
    try:
        connection.adapter.execute(rewritten.expression.sql(dialect="duckdb"), bindings).fetchall()
    except _NativeRetrySignal as exc:
        failures = []
        for index, _ in exc.failures:
            policy = planner.policy_by_index(index)
            if policy is None or policy.on_fail is not Resolution.RETRY:
                raise RuntimeError("Tool sink preflight returned an unknown RETRY policy") from exc
            failures.append(RetryFailure(index, policy.retry_message, policy))
        raise _original_retry_indices(connection.planner, RetryViolation(failures)) from exc


def _original_retry_indices(original: Planner, violation: RetryViolation) -> RetryViolation:
    policies = list(zip(original.policy_indices(), original.policies(), strict=True))
    failures = []
    for failure in violation.failures:
        match = next(
            ((index, policy) for index, policy in policies if policy == failure.policy), None
        )
        if match is None:
            raise RuntimeError("Tool publication returned an unknown RETRY policy")
        index, policy = match
        failures.append(RetryFailure(index, failure.message, policy))
    return RetryViolation(failures)


def _normalize_aggregate_aliases(sql: str) -> str:
    # SQLGlot emits LISTAGG, which sqlparser represents as a special expression
    # outside the core's Function aggregate traversal. DuckDB's equivalent
    # STRING_AGG spelling preserves the existing aggregate rewrite path. Work
    # on tokens so string literals, quoted identifiers and payloads stay intact.
    tokens = sqlglot.Tokenizer(dialect="duckdb").tokenize(sql)
    for index in range(len(tokens) - 2, -1, -1):
        token = tokens[index]
        if (
            token.text.upper() == "LISTAGG"
            and token.token_type is not TokenType.IDENTIFIER
            and tokens[index + 1].token_type is TokenType.L_PAREN
        ):
            sql = sql[: token.start] + "STRING_AGG" + sql[token.end + 1 :]
    return sql


def _execute_whole_output_remove(connection: Connection, sql: str, bindings: list[Any]):
    """Use DFC's terminal tuple predicates without changing registered policies.

    The native marker is a non-pushdown resolution that discards a complete
    candidate after its original group/frame predicate has been evaluated.
    The ordinary execute path preserves KILL/RETRY handling and joint priority.
    """
    original = connection.planner
    planner = Planner(dialect=original.dialect)
    planner.sync_catalog(connection._introspect_catalog(include_row_counts=True))
    # The validated flat model is followed only by a bijective output binding.
    # No downstream aggregate can combine surviving and removed model outputs.
    # This scoped declaration permits the core's existing derived-table route;
    # the terminal marker is not generally splittable for arbitrary SQL.
    planner.register_resolution_splittability("__passant_remove_output", splittable=True)
    aggregate_names = {
        row[0].lower()
        for row in connection.raw_connection.execute(
            "SELECT function_name FROM duckdb_functions() WHERE function_type = 'aggregate'"
        ).fetchall()
    }

    def terminal_policy(policy):
        expression = sqlglot.parse_one(policy.constraint, read="duckdb")
        contains_aggregate = any(
            isinstance(function, exp.AggFunc)
            or (
                function.name if isinstance(function, exp.Anonymous) else function.sql_name()
            ).lower()
            in aggregate_names
            for function in expression.find_all(exp.Func)
        )
        # REMOVE's row predicate must hold for every original contributor,
        # including NULL-valued rows. Explicit policy aggregates retain their
        # authored meaning and are evaluated over the complete group/frame.
        constraint = (
            policy.constraint if contains_aggregate else f"BOOL_AND(({policy.constraint}) IS TRUE)"
        )
        return replace(
            policy,
            constraint=constraint,
            on_fail=Resolution.UDF,
            udf_name="__passant_remove_output",
        )

    planner.register_policies(
        [
            terminal_policy(policy) if policy.on_fail is Resolution.REMOVE else policy
            for policy in original.policies()
        ]
    )
    try:
        connection.planner = planner
        return connection.execute(sql, params=bindings).fetchone()
    except RetryViolation as exc:
        raise _original_retry_indices(original, exc) from exc
    finally:
        connection.planner = original


def _validate_publication_shape(connection: Connection, sql: str) -> bool:
    """Reject rewrites that can change contributors but retain an external row.

    Ordinary SQL can recompute a scalar/window/aggregate after REMOVE. External
    payloads cannot be recomputed. Return True for models requiring terminal
    whole-output REMOVE rather than ordinary contributor filtering.
    """
    policies = connection.planner.policies()
    if not policies:
        return False
    query = sqlglot.parse_one(sql, read="duckdb")
    for subquery in query.find_all(exp.Subquery):
        if not isinstance(subquery.parent, (exp.From, exp.Join, exp.Lateral)):
            raise ValueError(
                "Tool provenance with policies does not support scalar/expression subqueries"
            )
    for nested in query.find_all(exp.Query):
        if nested is not query and not isinstance(
            nested.parent,
            (exp.Subquery, exp.CTE, exp.SetOperation, exp.From, exp.Join, exp.Lateral),
        ):
            raise ValueError(
                "Tool provenance with policies does not support scalar/expression subqueries"
            )
    if query.find(exp.Exists):
        raise ValueError(
            "Tool provenance with policies does not support scalar/expression subqueries"
        )
    if any(
        policy.on_fail not in (Resolution.KILL, Resolution.REMOVE, Resolution.RETRY)
        for policy in policies
    ):
        raise ValueError("Tool provenance cannot repair or recompute external output payloads")
    if not any(policy.on_fail is Resolution.REMOVE for policy in policies):
        return False
    aggregates = {
        row[0].lower()
        for row in connection.raw_connection.execute(
            "SELECT function_name FROM duckdb_functions() WHERE function_type = 'aggregate'"
        ).fetchall()
    }
    changes_contributors = (
        query.find(exp.Window, exp.Group, exp.Having, exp.Distinct, exp.SetOperation) is not None
    )
    changes_contributors |= any(
        isinstance(function, exp.AggFunc)
        or (function.name if isinstance(function, exp.Anonymous) else function.sql_name()).lower()
        in aggregates
        for function in query.find_all(exp.Func)
    )
    changes_contributors |= any(join.side for join in query.find_all(exp.Join))
    if not changes_contributors:
        return False
    # A terminal predicate may safely remove one flat group's/window's output.
    # Resolving an inner stage before a second aggregation could still retain
    # an external output with changed contributors, so do not use this path for
    # nested relational stages until the core can carry unresolved predicates.
    if (
        not isinstance(query, exp.Select)
        or query.find(exp.Subquery, exp.CTE, exp.Lateral, exp.Distinct, exp.SetOperation)
        or any(join.side for join in query.find_all(exp.Join))
    ):
        raise ValueError(
            "Tool provenance REMOVE requires a row-preserving model or a single flat "
            "aggregate/window SELECT; nested, set, distinct and outer-join models "
            "cannot recompute external payloads"
        )
    return True
