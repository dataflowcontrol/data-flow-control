"""Structured rewrite error exposure from Rust."""

import pytest

from passant.compat import DFCPolicy, PassantRewriteError, Resolution, SQLRewriter


def test_rewrite_error_exposes_kind_for_unsupported_statement():
    if PassantRewriteError is None:
        pytest.skip("Passant extension not built")

    rewriter = SQLRewriter()
    rewriter.execute("CREATE TABLE foo (id INTEGER)")
    rewriter.register_policy(
        DFCPolicy(
            sources=["foo"],
            constraint="max(foo.id) > 1",
            on_fail=Resolution.REMOVE,
        )
    )
    with pytest.raises(PassantRewriteError) as exc_info:
        rewriter.transform_query("CREATE TABLE leak AS SELECT * FROM foo")
    assert exc_info.value.kind == "unsupported_statement"
    assert "create_table" in str(exc_info.value)
