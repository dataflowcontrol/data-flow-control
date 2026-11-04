"""Source aggregate policies survive deterministic JSON sink projection layers."""
import duckdb
import pytest
from data_flow_control import Policy, dfc


@pytest.mark.parametrize("mixed,recipient,allowed", [
    (False, "legal@example.com", True),
    (True, "legal@example.com", False),
    (False, "other@example.com", False),
])
def test_grouped_json_projection_sink(mixed, recipient, allowed):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE cases (id VARCHAR, category VARCHAR)")
    raw.execute("INSERT INTO cases VALUES ('1','Discrimination'),('2','Discrimination')")
    if mixed:
        raw.execute("INSERT INTO cases VALUES ('3','Other')")
    raw.execute('CREATE TABLE emails (arguments JSON, "to" VARCHAR, body VARCHAR)')
    db = dfc(raw)
    db.register_policy(Policy.from_pgn('''
        SOURCE REQUIRED cases AS c SINK emails AS i
        CONSTRAINT BOOL_AND(c.category = 'Discrimination')
            AND i."to" = 'legal@example.com'
        ON FAIL RETRY('Wrong recipient or category.')
    '''))
    sql = f'''INSERT INTO emails (arguments, "to", body)
        SELECT p.args AS arguments,
          CASE WHEN json_type(p.args -> '/to') = 'VARCHAR'
            THEN TRY_CAST((p.args -> '/to') ->> '$' AS VARCHAR) END AS "to",
          p.args ->> '/body' AS body
        FROM (SELECT json_object('to', v.recipient, 'body', v.body) AS args
          FROM (SELECT '{recipient}' AS recipient, CAST(list(id) AS VARCHAR) AS body
            FROM cases) AS v) AS p'''
    if allowed:
        db.execute(sql)
        assert raw.execute('SELECT "to", body FROM emails').fetchall() == [
            (recipient, '[1, 2]')
        ]
    else:
        with pytest.raises(Exception, match='Wrong recipient or category'):
            db.execute(sql)
        assert raw.execute('SELECT * FROM emails').fetchall() == []
