from __future__ import annotations

from hashlib import md5

import sqlglot
from sqlglot import exp

from .policy import AggregateDFCPolicy, DFCPolicy, Resolution
from .sqlglot_utils import get_column_name, get_table_name_from_column


def get_policy_identifier(policy: DFCPolicy | AggregateDFCPolicy) -> str:
    digest = md5(
        f"{policy.sources}|{policy.sink}|{policy.constraint}|{policy.on_fail.value}".encode(),
        usedforsecurity=False,
    ).hexdigest()[:10]
    return f"policy_{digest}"


def _parse_expr(sql: str) -> exp.Expression:
    return sqlglot.parse_one(sql, read="duckdb")


def _wrap_parenthesized(expr: exp.Expression) -> exp.Expression:
    return exp.Paren(this=expr.copy())


def _combine_with_and(existing: exp.Expression | None, additions: list[exp.Expression]) -> exp.Expression | None:
    terms: list[exp.Expression] = []
    if existing is not None:
        terms.append(_wrap_parenthesized(existing))
    terms.extend(_wrap_parenthesized(expr) for expr in additions)
    if not terms:
        return existing
    combined = terms[0]
    for term in terms[1:]:
        combined = exp.and_(combined, term)
    return combined


def _wrap_kill_constraint(constraint_expr: exp.Expression) -> exp.Expression:
    return exp.Case(
        ifs=[exp.If(this=constraint_expr.copy(), true=exp.var("true"))],
        default=exp.Anonymous(this="KILL", expressions=[]),
    )


def transform_aggregations_to_columns(
    constraint_expr: exp.Expression,
    _source_tables: set[str],
) -> exp.Expression:
    def replace(node: exp.Expression) -> exp.Expression:
        if not isinstance(node, exp.AggFunc):
            return node

        sql_name = node.sql_name().upper() if hasattr(node, "sql_name") else node.key.upper()
        if sql_name in {"COUNT", "APPROX_COUNT_DISTINCT", "APPROX_DISTINCT"}:
            return exp.Literal.number(1)
        if sql_name in {"COUNT_IF", "COUNTIF"}:
            condition = node.this.copy() if node.this is not None else exp.false()
            return exp.Case(
                ifs=[exp.If(this=condition, true=exp.Literal.number(1))],
                default=exp.Literal.number(0),
            )
        if sql_name == "ARRAY_AGG":
            values = []
            if node.this is not None:
                values.append(node.this.copy())
            values.extend(expr.copy() for expr in node.expressions)
            return exp.Array(expressions=values)
        if node.this is not None:
            return node.this.copy()
        columns = list(node.find_all(exp.Column))
        if columns:
            return columns[0].copy()
        return exp.Literal.number(1)

    return constraint_expr.transform(replace, copy=True)


def ensure_columns_accessible(
    parsed: exp.Select,
    _constraint: exp.Expression,
    _source_tables: set[str],
) -> None:
    _ = parsed


def _column_sql(table_name: str, column_name: str) -> str:
    return f"{table_name}.{column_name}"


def ensure_subqueries_have_constraint_columns(
    parsed: exp.Select,
    policies: list[DFCPolicy],
    source_tables: set[str],
) -> None:
    source_order: list[str] = []
    for policy in policies:
        for source in policy.sources:
            source_lower = source.lower()
            if source_lower in source_tables and source_lower not in source_order:
                source_order.append(source_lower)
    for table_name in sorted(source_tables):
        if table_name not in source_order:
            source_order.append(table_name)

    needed: dict[str, set[str]] = {table: set() for table in source_order}
    for policy in policies:
        for table_name, column_names in getattr(policy, "_source_columns_needed", {}).items():
            if table_name in needed:
                needed[table_name].update(column_names)

    cte_selects: list[exp.Select] = []
    with_expr = parsed.args.get("with_")
    if with_expr is not None:
        for cte in with_expr.expressions:
            if isinstance(cte, exp.CTE) and isinstance(cte.this, exp.Select):
                cte_selects.append(cte.this)

    for sub_select in [subquery.this for subquery in parsed.find_all(exp.Subquery) if isinstance(subquery.this, exp.Select)] + cte_selects:
        available_tables = {
            table.name.lower(): table
            for table in sub_select.find_all(exp.Table)
            if table.name
        }
        existing = {
            expr.sql().lower()
            for expr in sub_select.expressions
            if isinstance(expr, (exp.Column, exp.Alias))
        }
        existing.update(
            expr.alias_or_name.lower()
            for expr in sub_select.expressions
            if getattr(expr, "alias_or_name", None)
        )
        for table_name in source_order:
            column_names = needed.get(table_name, set())
            if table_name not in available_tables:
                continue
            for column_name in sorted(column_names):
                sql = _column_sql(table_name, column_name)
                if sql.lower() in existing or column_name.lower() in existing:
                    continue
                sub_select.append("expressions", _parse_expr(sql))
                existing.add(sql.lower())
                existing.add(column_name.lower())


def _policy_expr_for_aggregation(policy: DFCPolicy) -> exp.Expression:
    return policy._constraint_parsed.copy()


def _policy_expr_for_scan(policy: DFCPolicy, source_tables: set[str]) -> exp.Expression:
    return transform_aggregations_to_columns(policy._constraint_parsed, source_tables)


def _append_valid_column(parsed: exp.Select, expressions: list[exp.Expression]) -> None:
    if not expressions:
        return
    if len(expressions) == 1:
        parsed.append("expressions", exp.alias_(_wrap_parenthesized(expressions[0]), "valid"))
        return

    valid_expr = exp.and_(_wrap_parenthesized(expressions[0]), _wrap_parenthesized(expressions[1]))
    for expr in expressions[2:]:
        valid_expr = exp.and_(valid_expr, _wrap_parenthesized(expr))
    parsed.append("expressions", exp.alias_(valid_expr, "valid"))


def _message_case_expr(expr: exp.Expression, description: str | None) -> exp.Expression:
    message = description or "Policy violation"
    return exp.Case(
        ifs=[exp.If(this=expr.copy(), true=exp.Literal.string(""))],
        default=exp.Literal.string(message),
    )


def _append_invalid_string_column(
    parsed: exp.Select,
    expressions: list[tuple[exp.Expression, str | None]],
) -> None:
    if not expressions:
        return
    if len(expressions) == 1:
        case_expr = _message_case_expr(expressions[0][0], expressions[0][1])
        parsed.append("expressions", exp.alias_(case_expr, "invalid_string"))
        return

    args: list[exp.Expression] = [exp.Literal.string(" | ")]
    for expr, description in expressions:
        args.append(exp.Nullif(this=_message_case_expr(expr, description), expression=exp.Literal.string("")))
    parsed.append(
        "expressions",
        exp.alias_(exp.Anonymous(this="CONCAT_WS", expressions=args), "invalid_string"),
    )


def apply_policy_constraints_to_aggregation(
    parsed: exp.Select,
    policies: list[DFCPolicy],
    _source_tables: set[str],
    **_kwargs,
) -> None:
    if not policies:
        return

    existing_having = parsed.args.get("having")
    existing_having_expr = existing_having.this.copy() if existing_having else None
    additions: list[exp.Expression] = []
    valid_exprs: list[exp.Expression] = []
    message_exprs: list[tuple[exp.Expression, str | None]] = []

    for policy in policies:
        expr = _policy_expr_for_aggregation(policy)
        if policy.on_fail == Resolution.REMOVE:
            additions.append(expr)
        elif policy.on_fail == Resolution.KILL:
            additions.append(_wrap_kill_constraint(expr))
        elif policy.on_fail == Resolution.INVALIDATE:
            valid_exprs.append(expr)
        elif policy.on_fail == Resolution.INVALIDATE_MESSAGE:
            message_exprs.append((expr, policy.description))

    combined = _combine_with_and(existing_having_expr, additions)
    if combined is not None:
        parsed.set("having", exp.Having(this=combined))
    _append_valid_column(parsed, valid_exprs)
    _append_invalid_string_column(parsed, message_exprs)


def apply_policy_constraints_to_scan(
    parsed: exp.Select,
    policies: list[DFCPolicy],
    source_tables: set[str],
    **_kwargs,
) -> None:
    if not policies:
        return

    existing_where = parsed.args.get("where")
    existing_where_expr = existing_where.this.copy() if existing_where else None
    additions: list[exp.Expression] = []
    valid_exprs: list[exp.Expression] = []
    message_exprs: list[tuple[exp.Expression, str | None]] = []

    for policy in policies:
        expr = _policy_expr_for_scan(policy, source_tables)
        if policy.on_fail == Resolution.REMOVE:
            additions.append(expr)
        elif policy.on_fail == Resolution.KILL:
            additions.append(_wrap_kill_constraint(expr))
        elif policy.on_fail == Resolution.INVALIDATE:
            valid_exprs.append(expr)
        elif policy.on_fail == Resolution.INVALIDATE_MESSAGE:
            message_exprs.append((expr, policy.description))

    combined = _combine_with_and(existing_where_expr, additions)
    if combined is not None:
        parsed.set("where", exp.Where(this=combined))
    _append_valid_column(parsed, valid_exprs)
    _append_invalid_string_column(parsed, message_exprs)


def _extract_source_aggregates_from_constraint(
    constraint_expr: exp.Expression,
    source: str,
) -> list[exp.AggFunc]:
    source_lower = source.lower()
    return [
        agg.copy()
        for agg in constraint_expr.find_all(exp.AggFunc)
        if any(
            get_table_name_from_column(column) == source_lower
            and column.find_ancestor(exp.AggFunc) is agg
            for column in agg.find_all(exp.Column)
        )
    ]


def _extract_sink_expressions_from_constraint(
    constraint_expr: exp.Expression,
    sink: str | None,
) -> list[exp.Expression]:
    if sink is None:
        return []
    sink_lower = sink.lower()
    expressions: list[exp.Expression] = []
    def _agg_refs_sink(agg: exp.AggFunc) -> bool:
        if isinstance(agg.this, exp.Column):
            if get_table_name_from_column(agg.this) == sink_lower:
                return True
            if get_table_name_from_column(agg.this) is None and get_column_name(agg.this).lower() == sink_lower:
                return True
        return any(
            get_table_name_from_column(column) == sink_lower
            and column.find_ancestor(exp.AggFunc) is agg
            for column in agg.find_all(exp.Column)
        )

    for node in constraint_expr.find_all(exp.Filter):
        if isinstance(node.this, exp.AggFunc) and _agg_refs_sink(node.this):
            expressions.append(node.copy())
    for node in constraint_expr.find_all(exp.AggFunc):
        if _agg_refs_sink(node):
            if node.find_ancestor(exp.Filter) is not None:
                continue
            expressions.append(node.copy())
    if expressions:
        return expressions
    for node in constraint_expr.find_all(exp.Column):
        if get_table_name_from_column(node) == sink_lower and node.find_ancestor(exp.AggFunc) is None:
            expressions.append(node.copy())
    return expressions


def _find_outer_aggregate_for_inner(
    constraint_expr: exp.Expression,
    inner_agg_sql: str,
) -> str | None:
    inner_sql_upper = inner_agg_sql.upper()
    for agg in constraint_expr.find_all(exp.AggFunc):
        agg_sql = agg.sql().upper()
        if inner_sql_upper in agg_sql and agg_sql != inner_sql_upper:
            return agg.sql_name().upper() if hasattr(agg, "sql_name") else agg.key.upper()
    return None


def apply_aggregate_policy_constraints_to_aggregation(
    parsed: exp.Select,
    policies: list[AggregateDFCPolicy],
    _source_tables: set[str],
    **_kwargs,
) -> None:
    for policy in policies:
        policy_id = get_policy_identifier(policy)
        temp_index = 1
        for source in policy.sources:
            for agg in _extract_source_aggregates_from_constraint(policy._constraint_parsed, source):
                parsed.append("expressions", exp.alias_(agg.copy(), f"_{policy_id}_tmp{temp_index}"))
                temp_index += 1


def apply_aggregate_policy_constraints_to_scan(
    parsed: exp.Select,
    policies: list[AggregateDFCPolicy],
    _source_tables: set[str],
    **_kwargs,
) -> None:
    for policy in policies:
        policy_id = get_policy_identifier(policy)
        temp_index = 1
        for sink_expr in _extract_sink_expressions_from_constraint(policy._constraint_parsed, policy.sink):
            expr = sink_expr.copy()
            if isinstance(expr, exp.Column):
                expr = exp.column(expr.name)
            parsed.append("expressions", exp.alias_(expr, f"_{policy_id}_tmp{temp_index}"))
            temp_index += 1


__all__ = [
    "_extract_sink_expressions_from_constraint",
    "_extract_source_aggregates_from_constraint",
    "_find_outer_aggregate_for_inner",
    "apply_aggregate_policy_constraints_to_aggregation",
    "apply_aggregate_policy_constraints_to_scan",
    "apply_policy_constraints_to_aggregation",
    "apply_policy_constraints_to_scan",
    "ensure_columns_accessible",
    "ensure_subqueries_have_constraint_columns",
    "get_policy_identifier",
    "transform_aggregations_to_columns",
]
