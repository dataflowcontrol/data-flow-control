"""Public API surface: intended exports and no legacy names."""

from __future__ import annotations

import passant


def test_public_exports():
    expected = {
        "dfc",
        "Dialect",
        "Policy",
        "Resolution",
        "RewriteOptions",
        "PassantRewriteError",
    }
    assert set(passant.__all__) == expected


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
    exported = set(dir(passant))
    assert legacy.isdisjoint(exported)
