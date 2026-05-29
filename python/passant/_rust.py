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
        raise ValueError("Dimensions must be provided as a list of table or subquery entries")
    try:
        return _passant.normalize_policy_dimensions_py(dimensions)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc


def normalize_policy_dimension_aliases(dimensions: list[str]) -> dict[str, str]:
    if not isinstance(dimensions, list):
        raise ValueError("Dimensions must be provided as a list of table or subquery entries")
    try:
        return _passant.normalize_policy_dimension_aliases_py(dimensions)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc


def normalize_policy_dimension_queries(dimensions: list[str]) -> dict[str, str]:
    if not isinstance(dimensions, list):
        raise ValueError("Dimensions must be provided as a list of table or subquery entries")
    try:
        return _passant.normalize_policy_dimension_queries_py(dimensions)
    except ValueError as exc:
        raise ValueError(str(exc)) from exc


def resolution_to_python_py(value: str) -> str:
    return _passant.resolution_to_python_py(value)


def resolution_to_python(value) -> str:
    if isinstance(value, str):
        if value.startswith("{"):
            return resolution_to_python_py(value)
        return value.upper()
    if isinstance(value, dict):
        if "Remove" in value:
            return "REMOVE"
        if "Kill" in value:
            return "KILL"
        if "Udf" in value:
            return f"UDF {value['Udf']}"
        if "RelationUdf" in value:
            return f"RELATION UDF {value['RelationUdf']}"
        return resolution_to_python_py(json.dumps(value))
    mapping = {
        "Remove": "REMOVE",
        "Kill": "KILL",
    }
    key = str(value)
    if key in mapping:
        return mapping[key]
    return key.upper()
