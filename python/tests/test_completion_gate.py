"""Completion-gated tests mirroring passant-core/tests/completion/.

Run with: uv run pytest -m completion
Default CI excludes these via: uv run pytest -m "not completion"
"""

import pytest

pytestmark = pytest.mark.completion


def test_count_if_scan_rewrites_to_case_when():
  """Port of sql_rewriter/test_rewriter.py::test_policy_scan_with_count_if."""
  from passant.compat import DFCPolicy, Resolution, SQLRewriter

  rewriter = SQLRewriter()
  rewriter.execute("CREATE TABLE foo (id INTEGER)")
  rewriter.register_policy(
      DFCPolicy(
          sources=["foo"],
          constraint="COUNT_IF(foo.id > 2) > 0",
          on_fail=Resolution.REMOVE,
      )
  )
  transformed = rewriter.transform_query("SELECT id FROM foo")
  assert (
      transformed
      == "SELECT id FROM foo WHERE CASE WHEN foo.id > 2 THEN 1 ELSE 0 END > 0"
  )


def test_delete_policy_removes_registered_policy():
  from passant.compat import DFCPolicy, Resolution, SQLRewriter

  rewriter = SQLRewriter()
  rewriter.execute("CREATE TABLE foo (id INTEGER)")
  policy = DFCPolicy(
      sources=["foo"],
      constraint="max(foo.id) > 1",
      on_fail=Resolution.REMOVE,
  )
  rewriter.register_policy(policy)
  removed = rewriter.delete_policy(
      sources=["foo"],
      constraint="max(foo.id) > 1",
      on_fail=Resolution.REMOVE,
  )
  assert removed is True
  assert rewriter.get_dfc_policies() == []


def test_pgn_policy_text_parses():
  from passant.compat import PgnPolicy

  policy = PgnPolicy.from_text(
      "PGN OVER SOURCE foo SINK reports "
      "AGGREGATE sum(foo.amount) CONSTRAINT sum(foo.amount) <= 1000 ON FAIL REMOVE"
  )
  assert "PGN" in policy.text
