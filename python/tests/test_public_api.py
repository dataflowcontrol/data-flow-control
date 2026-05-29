"""Public API surface: intended exports and no legacy names."""

from __future__ import annotations

import data_flow_control


def test_public_exports():
    expected = {
        "dfc",
        "Dialect",
        "Policy",
        "Resolution",
        "RewriteOptions",
        "PassantRewriteError",
    }
    assert set(data_flow_control.__all__) == expected


def test_legacy_names_not_exported():
    legacy = {
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
    assert legacy.isdisjoint(exported)
