import duckdb
import pytest

from data_flow_control import Policy, dfc
from data_flow_control.intermediates import IntermediateError


@pytest.fixture
def conn():
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE cases(id VARCHAR, category VARCHAR, priority VARCHAR, amount INT)")
        c.execute(
            "INSERT INTO cases VALUES ('A','Benefits','Low',10), ('B','Discrimination','High',20), ('C','Discrimination','Low',30)"
        )
        c.execute("CREATE TABLE updates(case_id VARCHAR, success BOOLEAN)")
        c.register_policy(
            Policy.from_pgn("""SOURCE REQUIRED cases AS c SINK updates AS u
            CONSTRAINT c.id = u.case_id AND BOOL_AND(c.category <> 'Discrimination' OR c.priority = 'High')
            AND u.success ON FAIL REMOVE""")
        )
        c.enable_intermediate_propagation()
        yield c


def test_normal_execute_carries_case_b_across_materialization(conn):
    conn.execute(
        "CREATE TEMP TABLE send_input AS SELECT id, 'body' AS body FROM cases WHERE id='B'"
    )
    conn.execute("CREATE TABLE send_output AS SELECT id, TRUE AS success FROM send_input")
    assert conn.fetchall("SELECT * FROM send_output") == [("B", True)]
    assert conn.fetchall("INSERT INTO updates SELECT 'A', success FROM send_output") == [(0,)]
    assert conn.fetchall("INSERT INTO updates SELECT id, success FROM send_output") == [(1,)]
    assert conn.fetchall("SELECT * FROM updates") == [("B", True)]


def test_grouped_joint_expression_and_conflicting_case_identity(conn):
    conn.execute(
        "CREATE TABLE combined AS SELECT 'B' AS id, COUNT(*) AS n FROM cases WHERE id IN ('A','B')"
    )
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM combined") == [(0,)]
    conn.execute("CREATE TABLE wrong AS SELECT id FROM cases WHERE id='C'")
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM wrong") == [(0,)]


def test_fanout_self_join_preserves_whole_input_state(conn):
    conn.execute("CREATE TABLE sent AS SELECT id FROM cases WHERE id='B'")
    conn.execute("CREATE TABLE fanout AS SELECT a.id FROM sent a CROSS JOIN cases b")
    # Additional source contributors correctly conflict even though the public
    # projected value remains B.
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM fanout") == [(0,)]
    conn.execute("CREATE TABLE diamond AS SELECT a.id FROM sent a JOIN sent b ON a.id=b.id")
    assert conn.fetchall("INSERT INTO updates SELECT id, TRUE FROM diamond") == [(1,)]


def test_literal_cannot_forge_required_source_and_metadata_is_hidden(conn):
    assert conn.fetchall("INSERT INTO updates SELECT 'B', TRUE") == [(0,)]
    conn.execute("CREATE TABLE sent AS SELECT id FROM cases WHERE id='B'")
    result = conn.execute("SELECT s.* FROM sent s")
    assert [c[0] for c in result.description] == ["id"]
    assert result.fetchone() == ("B",)
    assert len(result.intermediates) == 1
    assert conn.last_intermediate_result() is result
    with pytest.raises(IntermediateError, match="reserved"):
        conn.execute("SELECT _dfc_intermediate_state FROM sent")


def test_unsupported_queries_fail_without_effects(conn):
    for sql in [
        "UPDATE cases SET amount=0",
        "DELETE FROM cases",
        "CREATE TABLE nope AS SELECT DISTINCT id FROM cases",
        "INSERT INTO updates SELECT id, TRUE FROM (SELECT * FROM cases) t",
    ]:
        with pytest.raises(IntermediateError):
            conn.execute(sql)
    assert conn.fetchall("SELECT COUNT(*) AS n FROM cases") == [(3,)]
    assert conn.fetchall("SELECT * FROM updates") == []
