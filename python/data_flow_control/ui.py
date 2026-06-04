"""UI resolution handler types and DuckDB stream helpers.

Stream rows are written as tab-separated values. Avoid embedded tabs or newlines
in string columns; NULL is not represented robustly in the MVP TSV format.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from typing import Any, Callable

UiViolationHandler = Callable[["UiViolationEvent"], dict[str, Any] | None]


@dataclass(frozen=True)
class UiViolationEvent:
    constraint: str
    description: str | None
    column_names: list[str]
    values: list[Any]
    row: dict[str, Any]
    source_columns: list[str]
    output_columns: list[str]
    stream_endpoint: str


def format_stream_cell(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def write_ui_stream_row(
    stream_endpoint: str,
    column_names: list[str],
    values: list[Any],
) -> None:
    if len(column_names) != len(values):
        raise ValueError("column_names and values must have the same length")
    line = "\t".join(format_stream_cell(value) for value in values)
    with open(stream_endpoint, "a", encoding="utf-8") as handle:
        handle.write(line)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())


def build_ui_approval_event(args: tuple[Any, ...]) -> UiViolationEvent:
    """Parse `passant_ui_approve(col..., constraint, description, column_names_json)`."""
    if len(args) < 3:
        raise ValueError("passant_ui_approve requires column values plus three metadata arguments")
    column_values = list(args[:-3])
    constraint = str(args[-3])
    description_raw = args[-2]
    description = None if description_raw in (None, "") else str(description_raw)
    column_names_json = str(args[-1])
    column_names: list[str] = json.loads(column_names_json)
    if len(column_names) != len(column_values):
        raise ValueError("column_names JSON length does not match column value count")
    source_columns = [name for name in column_names if "." in name]
    output_columns = [name for name in column_names if "." not in name]
    row = dict(zip(column_names, column_values, strict=True))
    return UiViolationEvent(
        constraint=constraint,
        description=description,
        column_names=column_names,
        values=column_values,
        row=row,
        source_columns=source_columns,
        output_columns=output_columns,
        stream_endpoint="",
    )


def build_ui_violation_event(
    args: tuple[Any, ...],
) -> UiViolationEvent:
    if len(args) < 4:
        raise ValueError(
            "address_violating_rows requires column values plus four trailing metadata arguments"
        )
    column_values = list(args[:-4])
    constraint = str(args[-4])
    description_raw = args[-3]
    description = None if description_raw in (None, "") else str(description_raw)
    column_names_json = str(args[-2])
    stream_endpoint = str(args[-1])
    column_names: list[str] = json.loads(column_names_json)
    if len(column_names) != len(column_values):
        raise ValueError("column_names JSON length does not match column value count")
    source_columns = [name for name in column_names if "." in name]
    output_columns = [name for name in column_names if "." not in name]
    row = dict(zip(column_names, column_values, strict=True))
    return UiViolationEvent(
        constraint=constraint,
        description=description,
        column_names=column_names,
        values=column_values,
        row=row,
        source_columns=source_columns,
        output_columns=output_columns,
        stream_endpoint=stream_endpoint,
    )


def merge_handler_row(
    event: UiViolationEvent,
    corrected: dict[str, Any],
) -> list[Any]:
    merged: list[Any] = []
    for name, original in zip(event.column_names, event.values, strict=True):
        if name in corrected:
            merged.append(corrected[name])
        else:
            merged.append(original)
    return merged
