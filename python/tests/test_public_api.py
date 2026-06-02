"""Public API surface: intended exports and forbidden extra names."""

from __future__ import annotations

import data_flow_control


def test_public_exports():
    expected = {
        "dfc",
        "Dialect",
        "Policy",
        "Resolution",
        "RewriteOptions",
        "UiUpdateMode",
        "PassantRewriteError",
        "UiViolationEvent",
    }
    assert set(data_flow_control.__all__) == expected


def test_forbidden_names_not_exported():
    forbidden = {
        "SQLRewriter",
        "DFCPolicy",
        "AggregateDFCPolicy",
        "AggregatePolicy",
        "INVALIDATE",
        "UDF",
        "LLM",
        "compat",
        "connect",
        "wrap",
        "Connection",
        "Planner",
        "SUPPORTED_DIALECTS",
    }
    exported = set(dir(data_flow_control))
    assert forbidden.isdisjoint(exported)
