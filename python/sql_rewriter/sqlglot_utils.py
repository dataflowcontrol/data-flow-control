from __future__ import annotations

from sqlglot import exp


def get_column_name(column: exp.Column | exp.Identifier | str) -> str:
    if isinstance(column, exp.Column):
        return column.name
    if isinstance(column, exp.Identifier):
        return column.name
    if isinstance(column, str):
        return column
    return str(column)


def get_table_name_from_column(column: exp.Column) -> str | None:
    table = column.table
    if table is None:
        return None
    if table == "":
        return None
    if isinstance(table, exp.Identifier):
        return table.name.lower()
    if isinstance(table, str):
        return table.lower() or None
    name = getattr(table, "name", None)
    if isinstance(name, str):
        return name.lower()
    this = getattr(table, "this", None)
    if isinstance(this, str):
        return this.lower()
    return str(table).lower()


__all__ = ["get_column_name", "get_table_name_from_column"]
