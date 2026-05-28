"""Policy parsing and validation."""

from __future__ import annotations

import pytest

from passant import Policy


def test_aggregate_keyword_rejected_by_rust_parser():
    with pytest.raises(
        ValueError, match="aggregate policies are not supported|invalid policy syntax"
    ):
        Policy.from_policy_str(
            "AGGREGATE SOURCE foo SINK reports CONSTRAINT sum(foo.id) > 1 ON FAIL REMOVE"
        )
