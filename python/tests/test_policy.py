"""Policy parsing and validation."""

from __future__ import annotations

import pytest

from data_flow_control import Policy


def test_aggregate_keyword_rejected_by_rust_parser():
    with pytest.raises(ValueError, match="AGGREGATE clause was removed|invalid policy syntax"):
        Policy.from_pgn(
            "AGGREGATE SOURCE foo SINK reports CONSTRAINT sum(foo.id) > 1 ON FAIL REMOVE"
        )
