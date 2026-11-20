"""Optional public bridge checks when SQL-tool PR #7 is combined with this PR."""

import duckdb
import pytest

from data_flow_control import Policy, dfc

provenance = pytest.importorskip("data_flow_control.tool_provenance")


@pytest.mark.parametrize(
    "sql",
    [
        "WITH g AS (SELECT id, SUM(amount) AS total FROM cases GROUP BY id) SELECT * FROM g",
        "SELECT id, SUM(amount) OVER (PARTITION BY id) AS total FROM cases",
    ],
)
def test_native_model_states_bind_to_actual_outputs_and_enforce_later_sink(sql):
    with dfc(duckdb.connect()) as c:
        c.execute("CREATE TABLE cases(id INT, amount INT)")
        c.execute("INSERT INTO cases VALUES (1,10),(2,20)")
        c.execute("CREATE TABLE observations(message VARCHAR, total INT)")
        c.execute("CREATE TABLE final(id INT, total INT)")
        c.register_policy(
            Policy.from_pgn("""SOURCE REQUIRED cases SINK final
            CONSTRAINT cases.id=final.id AND SUM(cases.amount)=final.total ON FAIL REMOVE""")
        )
        c.enable_intermediate_propagation()
        c.execute("CREATE TABLE selected AS SELECT * FROM cases")
        model = provenance.ToolProvenance(sql.replace("FROM cases", "FROM selected"), ("id",))
        outputs = [
            provenance.ToolOutput((2,), {"message": "m2", "total": 20}),
            provenance.ToolOutput((1,), {"message": "m1", "total": 10}),
        ]
        modeled = c.execute(model.sql)
        states = model.bind_intermediates(modeled, outputs)
        c.store_intermediate_outputs("observations", [("m2", 20), ("m1", 10)], states)
        assert c.fetchall(
            "INSERT INTO final SELECT 1,total FROM observations WHERE message='m2'"
        ) == [(0,)]
        assert c.fetchall(
            "INSERT INTO final SELECT 2,total FROM observations WHERE message='m2'"
        ) == [(1,)]
