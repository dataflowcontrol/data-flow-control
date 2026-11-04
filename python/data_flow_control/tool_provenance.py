"""Trusted SQL models for publishing externally computed tool outputs.

The model is relational SQL, not an implementation of the external tool. See
``developer-docs/tool-provenance.md`` for the trust and snapshot contract.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

import sqlglot
from sqlglot import exp
from sqlglot.optimizer.qualify import qualify
from sqlglot.optimizer.merge_subqueries import merge_subqueries
from sqlglot.tokens import TokenType

from .adapters.duckdb import DuckDBAdapter
from .connection import Connection


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
            sql = merge_subqueries(sqlglot.parse_one(_number_parameters(sql), read="duckdb")).sql(
                dialect="duckdb"
            )
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
