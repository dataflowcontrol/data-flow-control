from __future__ import annotations

import json

from . import _passant


def parse_policy_to_json(policy_str: str) -> dict:
    return json.loads(_passant.parse_policy_to_json(policy_str))


def validate_constraint_expression(sql: str, label: str) -> None:
    _passant.validate_constraint_expression_py(sql, label)


def normalize_policy_source_aliases(sources: list[str]) -> dict[str, str]:
    if sources is None:
        raise ValueError("Sources must be provided (use an empty list for no sources)")
    if not isinstance(sources, list):
        raise ValueError("Sources must be provided as a list of table names")
    try:
        return _passant.normalize_policy_source_aliases_py(sources)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc


def normalize_policy_sources(sources: list[str]) -> list[str]:
    if sources is None:
        raise ValueError("Sources must be provided (use an empty list for no sources)")
    if not isinstance(sources, list):
        raise ValueError("Sources must be provided as a list of table names")
    try:
        return _passant.normalize_policy_sources_py(sources)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc


def normalize_policy_dimensions(dimensions: list[str]) -> list[str]:
    if not isinstance(dimensions, list):
        raise ValueError("Dimensions must be provided as a list of qualified column names")
    try:
        return _passant.normalize_policy_dimensions_py(dimensions)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc


def resolution_to_python(value) -> str:
    if isinstance(value, str):
        return value.upper()
    mapping = {
        "Remove": "REMOVE",
        "Kill": "KILL",
    }
    return mapping.get(str(value), str(value).upper())
