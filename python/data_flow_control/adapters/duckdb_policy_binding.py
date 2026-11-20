"""Registration binding on a data-free DuckDB connection, not on the live database."""

from __future__ import annotations

import duckdb
import sqlglot
from sqlglot import exp

from .._passant import PassantRewriteError


class _Deferred(Exception):
    """An expression has no faithful, safe registration-time context."""


def _quote(name):
    return '"' + name.replace('"', '""') + '"'


def _parts(name):
    node = sqlglot.parse_one(name, into=exp.Table, read="duckdb")
    if not node.parts or not all(isinstance(part, exp.Identifier) for part in node.parts):
        raise _Deferred("relation identifier is not a plain catalog name")
    return tuple(part.name for part in node.parts)


def _parse(sql):
    try:
        return sqlglot.parse_one(sql, read="duckdb")
    except sqlglot.errors.SqlglotError as error:
        raise _Deferred("cannot inspect this SQL form safely") from error


def _read_only(sql, conn):
    # Never send a policy-supplied string to execute(). A second parser makes
    # statement shape independent of sqlglot's inspection.
    statements = conn.extract_statements(sql)
    if len(statements) != 1 or statements[0].type != duckdb.StatementType.SELECT:
        raise _Deferred("expected a single SELECT")
    return conn.sql(sql)


def _native_error(label, error, sql):
    exc = PassantRewriteError(f"{label} failed DuckDB registration binding: {error}\nSQL: {sql}")
    exc.kind = "backend_binding"
    return exc


def _function_names(conn):
    return {
        name.lower()
        for (name,) in conn.execute(
            "SELECT DISTINCT function_name FROM system.main.duckdb_functions()"
        ).fetchall()
    }


class _Binder:
    def __init__(self, sandbox, source_conn, catalog):
        self.sandbox = sandbox
        self.source_conn = source_conn
        self.catalog = catalog
        self.tables = {name.lower(): (name, entry) for name, entry in catalog["tables"].items()}
        self.cloned = set()
        self.live_only_functions = None

    def _check(self, node):
        if any(isinstance(expr, (exp.DDL, exp.DML, exp.Command)) for expr in node.walk()):
            raise _Deferred("query includes a non-read-only operation")
        if any(not isinstance(table.this, exp.Identifier) for table in node.find_all(exp.Table)):
            # DuckDB can invoke user callbacks or infer a file's schema while
            # binding a table function. Never bind arbitrary table functions.
            raise _Deferred("table function outside the safe binding subset")
        names = {function.name.lower() for function in node.find_all(exp.Anonymous)}
        if names and self.live_only_functions is None:
            declared = {
                entry["name"].lower() for entry in self.catalog.get("aggregate_functions", [])
            }
            self.live_only_functions = (
                _function_names(self.source_conn) | declared
            ) - _function_names(self.sandbox)
        if names & (self.live_only_functions or set()):
            raise _Deferred("requires functions defined only on the live connection")

    def _lookup(self, parts):
        name = ".".join(parts).lower()
        candidates = [name]
        if len(parts) == 1:
            candidates.extend(
                f"{schema.lower()}.{name}" for schema in self.catalog.get("search_path", [])
            )
        elif len(parts) == 2 and parts[0].lower() == self.catalog.get("default_schema", "").lower():
            candidates.append(parts[1].lower())
        return next((self.tables[key] for key in candidates if key in self.tables), None)

    def _clone(self, table):
        name, entry = table
        columns = entry["columns"]
        if name in self.cloned:
            return columns
        parts = name.split(".")
        if len(parts) > 2:
            raise _Deferred("catalog relation without a reproducible schema")
        if len(parts) == 2:
            self.sandbox.execute(f"CREATE SCHEMA IF NOT EXISTS {_quote(parts[0])}")
        qualified = ".".join(map(_quote, parts))
        try:
            types = [
                # Metadata only. Parse types separately and interpolate the
                # backend's canonical type rendering, not a policy string.
                str(self.sandbox.sqltype(entry["types"][column]))
                for column in columns
            ]
        except (KeyError, duckdb.Error) as error:
            raise _Deferred("type not reproducible in the isolated catalog") from error
        declarations = ", ".join(
            f"{_quote(column)} {column_type}"
            for column, column_type in zip(columns, types, strict=True)
        )
        statement = f"CREATE TABLE {qualified} ({declarations})"
        parsed = self.sandbox.extract_statements(statement)
        if len(parsed) != 1 or parsed[0].type != duckdb.StatementType.CREATE:
            raise _Deferred("catalog type rendering does not form a single CREATE TABLE")
        self.sandbox.execute(statement)
        self.cloned.add(name)
        return columns

    def _clone_referenced(self, node):
        for table in node.find_all(exp.Table):
            if table.this.quoted and any(separator in table.name for separator in ("/", "\\")):
                raise _Deferred("possible external relation")
            metadata = self._lookup(tuple(part.name for part in table.parts))
            if metadata is not None:
                self._clone(metadata)
            # CTE names need not exist in the catalog. If it is a missing real
            # table, the native static check or DuckDB itself will reject it.

    def dimensions(self, policy):
        results, columns = {}, {}
        for alias, sql in (policy.dimension_queries or {}).items():
            try:
                node = _parse(sql)
                self._check(node)
                self._clone_referenced(node)
                columns[alias] = _read_only(sql, self.sandbox).columns
                results[alias] = "duckdb bound"
            except _Deferred as error:
                results[alias] = f"static only: {error}"
            except duckdb.Error as error:
                raise _native_error(f"DIMENSION {alias!r}", error, sql) from error
        return results, columns

    def constraint(self, policy, sql, aggregate, dimension_columns):
        node = _parse(sql)
        self._check(node)
        self._clone_referenced(node)
        qualifiers = {
            tuple(part.name.lower() for part in column.parts[:-1])
            for column in node.find_all(exp.Column)
            if len(column.parts) > 1
        }
        if ("_output_",) in qualifiers and policy.sink is None:
            raise _Deferred("_OUTPUT_ columns depend on a future query")
        from_items, group_items, used = [], [], set()

        def add_relation(sql, qualifier, columns):
            normalized = tuple(part.lower() for part in qualifier)
            if normalized in used:
                return
            used.add(normalized)
            from_items.append(sql)
            prefix = ".".join(map(_quote, qualifier))
            group_items.extend(f"{prefix}.{_quote(column)}" for column in columns)

        def add_table(table, alias=None, *, force=False, allow_short=True):
            parts = _parts(table)
            prefix = (_parts(alias)[-1],) if alias else parts
            normalized = tuple(part.lower() for part in prefix)
            if not force and normalized not in qualifiers and (
                alias or not allow_short or normalized[-1:] not in qualifiers
            ):
                return
            # The native static layer checked direct relation declarations.
            metadata = self._lookup(parts)
            if metadata is None:
                raise _Deferred("catalog relation cannot be reproduced")
            columns = self._clone(metadata)
            table_sql = ".".join(map(_quote, parts))
            add_relation(
                f"{table_sql} AS {_quote(prefix[0])}" if alias else table_sql, prefix, columns
            )

        for source in policy.sources:
            source_parts = tuple(part.lower() for part in _parts(source))
            aliased = any(
                tuple(part.lower() for part in _parts(base)) == source_parts
                for base in (policy.source_aliases or {}).values()
            )
            add_table(source, force=not aliased, allow_short=not aliased)
        for alias, source in (policy.source_aliases or {}).items():
            add_table(source, alias)
        if policy.sink:
            add_table(policy.sink, policy.sink_alias)
            if policy.sink_alias:
                add_table(policy.sink, allow_short=False)
            add_table(policy.sink, "_OUTPUT_")
        for dimension in policy.dimensions or []:
            dimension_parts = tuple(part.lower() for part in _parts(dimension))
            aliased = any(
                tuple(part.lower() for part in _parts(base)) == dimension_parts
                for alias, base in (policy.dimension_aliases or {}).items()
                if alias not in (policy.dimension_queries or {})
            )
            add_table(dimension, allow_short=not aliased)
        for alias, dimension in (policy.dimension_aliases or {}).items():
            if alias not in (policy.dimension_queries or {}):
                add_table(dimension, alias)
        for alias, query in (policy.dimension_queries or {}).items():
            alias_name = _parts(alias)[-1]
            if (alias_name.lower(),) in qualifiers:
                if alias not in dimension_columns:
                    raise _Deferred(f"requires an unbound DIMENSION {alias!r}")
                # An outer derived-table alias provides column bindings only;
                # do not expose it as a CTE or a physical FROM target.
                add_relation(
                    f"({query}) AS {_quote(alias_name)}", (alias_name,), dimension_columns[alias]
                )

        query = f'SELECT ({sql}) AS "__passant_registration_constraint"'
        if from_items:
            query += " FROM " + ", ".join(from_items)
        if aggregate and group_items:
            # The witness need not reproduce multiplicity; it must permit the
            # same typed references beside top-level policy aggregates.
            query += " GROUP BY " + ", ".join(dict.fromkeys(group_items))
        result = _read_only(query, self.sandbox)
        datatype = str(result.types[0])
        if datatype not in {"BOOLEAN", '"NULL"', "NULL"}:
            raise _native_error("CONSTRAINT", f"expected boolean, got {datatype}", sql)
        return "duckdb bound"


def validate_policy_bindings(source_conn, catalog, policies, inputs):
    """Run the binder, never evaluate policy results or expose live data/functions."""
    config = {
        "enable_external_access": False,
        "autoload_known_extensions": False,
        "autoinstall_known_extensions": False,
        "python_enable_replacements": False,
    }
    with duckdb.connect(":memory:", config=config) as sandbox:
        binder = _Binder(sandbox, source_conn, catalog)
        reports = []
        for policy, (sql, aggregate) in zip(policies, inputs, strict=True):
            dimensions, columns = binder.dimensions(policy)
            try:
                constraint = binder.constraint(policy, sql, aggregate, columns)
            except _Deferred as error:
                constraint = f"static only: {error}"
            except duckdb.Error as error:
                raise _native_error("CONSTRAINT", error, sql) from error
            reports.append({"constraint": constraint, "dimensions": dimensions})
        return reports
