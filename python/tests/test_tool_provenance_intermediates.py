"""Optional PR #7 / PR #8 integration through trusted harness boundaries.

Run with PR #8's intermediates.py and intermediate_policy.py available alongside
PR #7. The standalone branch skips this module. This example derives occurrence
correspondence from the model query's projected existing input IDs; it is not a
general SQL lineage inference implementation. Its database snapshot is immutable
through capture, model evaluation and publication.
"""

import duckdb
import pytest

from data_flow_control import Policy, Resolution, ToolOutput, ToolProvenance, dfc

intermediate_policy = pytest.importorskip("data_flow_control.intermediate_policy")


@pytest.fixture
def harness():
    connection = dfc(duckdb.connect())
    connection.execute("CREATE TABLE Cases (id INTEGER, category VARCHAR, priority VARCHAR)")
    connection.execute(
        "INSERT INTO Cases VALUES (1, 'Discrimination', 'High'), (2, 'Benefits', 'Low'), "
        "(3, 'Discrimination', 'Low'), (4, 'Benefits', 'High')"
    )
    connection.execute("CREATE TABLE ToolInput (id INTEGER, copies INTEGER)")
    connection.execute("INSERT INTO ToolInput VALUES (7, 2), (8, 1), (9, 1)")
    connection.execute(
        "CREATE TABLE ToolOutput (message VARCHAR, sent BOOLEAN, contributors INTEGER)"
    )
    policy = Policy(
        sources=["Cases"],
        sink="ToolOutput",
        constraint=(
            "BOOL_AND(Cases.category <> 'Discrimination' OR Cases.priority = 'High') "
            "AND COUNT(Cases.id) = ToolOutput.contributors AND ToolOutput.sent"
        ),
        on_fail=Resolution.KILL,
    )
    compiled = intermediate_policy.compile_policy_intermediates(connection.raw_connection, policy)
    # One tool input is already an aggregate of two source contributors. The
    # tool adapter receives only the resulting whole state, never those rows.
    inputs = {
        7: compiled.plan.capture(
            "Cases", "SELECT * FROM Cases WHERE id IN (1, 2)", identity_columns=["id"]
        ),
        8: compiled.plan.capture(
            "Cases", "SELECT * FROM Cases WHERE id = 3", identity_columns=["id"]
        ),
        9: compiled.plan.capture(
            "Cases", "SELECT * FROM Cases WHERE id = 4", identity_columns=["id"]
        ),
    }
    yield connection, compiled, inputs
    connection.close()


def publish_with_explicit_harness(
    connection, compiled, inputs, model, outputs, input_columns=("id",)
):
    """Example host integration, deliberately not a public inference API.

    The host knows which projected columns identify contributing tool inputs.
    SQL determines the surviving modeled occurrences and their join partners;
    the host must not zip the result order with the actual output order.
    """
    cursor = connection.raw_connection.execute(model.sql)
    columns = [entry[0] for entry in cursor.description]
    states = {}
    for values in cursor.fetchall():
        modeled = dict(zip(columns, values, strict=True))
        key = tuple(modeled[column] for column in model.key_columns)
        contributors = [inputs[modeled[column]] for column in input_columns]
        # Projection/fan-out copy the entire state. A join merges whole states.
        states[key] = (
            contributors[0] if len(contributors) == 1 else compiled.plan.merge(*contributors)
        )
    for output in outputs:
        if compiled.evaluate(states[output.key], sink=dict(output.values)) is not True:
            # This example policy declares KILL. The trusted host applies it
            # before any database publication; a real side-effect guard must
            # similarly run before the external side effect itself.
            raise ValueError("KILL: intermediate policy rejected the actual output")
    inserted = model.insert_outputs(connection, "ToolOutput", outputs)
    return inserted, states


@pytest.mark.parametrize("fanout", [False, True])
def test_whole_aggregate_state_survives_sql_tool_output(harness, fanout):
    connection, compiled, inputs = harness
    if fanout:
        model = ToolProvenance(
            "SELECT i.*, n.copy FROM ToolInput i CROSS JOIN (VALUES (1), (2)) n(copy) "
            "WHERE i.id = 7 AND n.copy <= i.copies",
            ("id", "copy"),
        )
        keys = [(7, 2), (7, 1)]  # actual output order differs from model order
    else:
        model = ToolProvenance("SELECT * FROM ToolInput WHERE id = 7", ("id",))
        keys = [(7,)]
    outputs = [
        ToolOutput(key, {"message": "same", "sent": True, "contributors": 2}) for key in keys
    ]
    count, states = publish_with_explicit_harness(connection, compiled, inputs, model, outputs)
    assert count == len(outputs)
    assert all(state is inputs[7] for state in states.values())
    # A downstream fan-in keeps the aggregate's two original contributors;
    # copying the state into two tool outputs does not double COUNT to four.
    downstream = compiled.plan.merge(*(states[output.key] for output in outputs))
    assert compiled.evaluate(downstream, sink={"sent": True, "contributors": 2}) is True
    assert connection.raw_connection.execute("SELECT COUNT(*) FROM ToolOutput").fetchone() == (
        count,
    )


@pytest.mark.parametrize("input_id,contributors,sent", [(8, 1, True), (7, 1, True), (7, 2, False)])
def test_propagated_expression_and_actual_sink_failure_prevent_publication(
    harness, input_id, contributors, sent
):
    connection, compiled, inputs = harness
    model = ToolProvenance(f"SELECT * FROM ToolInput WHERE id = {input_id}", ("id",))
    outputs = [
        ToolOutput((input_id,), {"message": "result", "sent": sent, "contributors": contributors})
    ]
    with pytest.raises(ValueError, match="KILL"):
        publish_with_explicit_harness(connection, compiled, inputs, model, outputs)
    assert connection.raw_connection.execute("SELECT COUNT(*) FROM ToolOutput").fetchone() == (0,)


def test_projected_join_partners_merge_whole_input_states(harness):
    connection, compiled, inputs = harness
    model = ToolProvenance(
        "SELECT a.id AS left_id, b.id AS right_id FROM ToolInput a "
        "JOIN ToolInput b ON a.id = 7 AND b.id = 9",
        ("left_id", "right_id"),
    )
    outputs = [ToolOutput((7, 9), {"message": "joined", "sent": True, "contributors": 3})]
    count, states = publish_with_explicit_harness(
        connection, compiled, inputs, model, outputs, ("left_id", "right_id")
    )
    assert count == 1
    assert compiled.evaluate(states[(7, 9)], sink={"sent": True, "contributors": 3}) is True


@pytest.fixture
def automatic_harness():
    """Latest PR #8 route: no manual capture or source-state lookup."""
    connection = dfc(duckdb.connect())
    if not hasattr(connection, "enable_intermediate_propagation"):
        connection.close()
        pytest.skip("Requires PR #8's automatic execution adapter")
    connection.execute(
        "CREATE TABLE Cases(id INTEGER, case_id INTEGER, category VARCHAR, priority VARCHAR)"
    )
    connection.execute(
        "INSERT INTO Cases VALUES (1, 7, 'Discrimination', 'High'), (2, 7, 'Benefits', 'Low')"
    )
    # This declared input relation represents external delivery configuration;
    # its rows multiply outputs but are not a fabricated provenance-pair table.
    connection.execute("CREATE TABLE Channels(channel VARCHAR)")
    connection.execute("INSERT INTO Channels VALUES ('email'), ('archive')")
    connection.execute(
        "CREATE TABLE ToolOutput(message VARCHAR, sent BOOLEAN, contributors INTEGER)"
    )
    connection.execute("CREATE TABLE Final(contributors INTEGER, sent BOOLEAN)")
    connection.register_policy(
        Policy(
            sources=["Cases"],
            sink="Final",
            constraint=(
                "BOOL_AND(Cases.category <> 'Discrimination' OR Cases.priority = 'High') "
                "AND COUNT(Cases.id) = Final.contributors AND Final.sent"
            ),
            on_fail=Resolution.KILL,
        )
    )
    connection.enable_intermediate_propagation()
    connection.execute(
        "CREATE TABLE ToolInput AS SELECT case_id AS id, COUNT(*) AS contributors "
        "FROM Cases GROUP BY case_id"
    )
    yield connection
    connection.close()


def publish_with_automatic_harness(connection, model, outputs):
    """Minimal host bridge for SQL forms supported by automatic propagation.

    Unlike publish_with_explicit_harness, this uses the normal execution result's
    automatically derived states and needs no declaration of input-ID columns.
    It records successful observations; downstream ordinary SQL enforces sinks.
    """
    modeled = connection.execute(model.sql)
    aligned_states = model.bind_intermediates(modeled, outputs)
    output_columns = [
        column[0]
        for column in connection.execute("SELECT * FROM ToolOutput WHERE FALSE").description
    ]
    connection.store_intermediate_outputs(
        "ToolOutput",
        [tuple(output.values[column] for column in output_columns) for output in outputs],
        aligned_states,
    )
    return dict(zip((output.key for output in outputs), aligned_states, strict=True))


def automatic_model(fanout):
    if fanout:
        return ToolProvenance(
            "SELECT i.id, i.contributors, c.channel FROM ToolInput i CROSS JOIN Channels c",
            ("id", "channel"),
        ), [(7, "email"), (7, "archive")]
    return ToolProvenance("SELECT * FROM ToolInput", ("id",)), [(7,)]


@pytest.mark.parametrize("fanout", [False, True])
def test_normal_execution_tool_bridge_and_downstream_policy(automatic_harness, fanout):
    connection = automatic_harness
    model, keys = automatic_model(fanout)
    outputs = [
        ToolOutput(key, {"message": "same", "sent": True, "contributors": 2})
        for key in reversed(keys)
    ]
    states = publish_with_automatic_harness(connection, model, outputs)
    assert set(states) == set(keys)
    # Two modeled occurrences copy the same indivisible aggregate state. The
    # downstream GROUP BY/global aggregate must keep COUNT at two, not four.
    assert connection.fetchall(
        "INSERT INTO Final SELECT MAX(contributors), BOOL_AND(sent) FROM ToolOutput"
    ) == [(1,)]
    assert connection.fetchall("SELECT * FROM Final") == [(2, True)]


@pytest.mark.parametrize("contributors,sent", [(1, True), (2, False)])
def test_normal_execution_rejects_bad_actual_sink(automatic_harness, contributors, sent):
    connection = automatic_harness
    model, keys = automatic_model(True)
    outputs = [
        ToolOutput(key, {"message": "observed", "sent": sent, "contributors": contributors})
        for key in reversed(keys)
    ]
    publish_with_automatic_harness(connection, model, outputs)
    # Observations are recorded, but they cannot authorize a later sink whose
    # values fail the policy inherited from the original Cases contributors.
    with pytest.raises(Exception, match="KILL|kill"):
        connection.execute(
            "INSERT INTO Final SELECT MAX(contributors), BOOL_AND(sent) FROM ToolOutput"
        )
    assert connection.fetchall("SELECT * FROM Final") == []


def test_normal_execution_rejects_missing_output_before_storage(automatic_harness):
    connection = automatic_harness
    model, keys = automatic_model(True)
    with pytest.raises(ValueError, match="match all modeled"):
        publish_with_automatic_harness(
            connection,
            model,
            [ToolOutput(keys[0], {"message": "partial", "sent": True, "contributors": 2})],
        )
    assert connection.fetchall("SELECT * FROM ToolOutput") == []


def test_automatic_binding_rejects_duplicate_actual_occurrences(automatic_harness):
    connection = automatic_harness
    model, keys = automatic_model(False)
    output = ToolOutput(keys[0], {"message": "duplicate", "sent": True, "contributors": 2})
    with pytest.raises(ValueError, match="unique"):
        publish_with_automatic_harness(connection, model, [output, output])
    assert connection.fetchall("SELECT * FROM ToolOutput") == []


def test_automatic_binding_rejects_duplicate_modeled_occurrences(automatic_harness):
    connection = automatic_harness
    model = ToolProvenance("SELECT i.id FROM ToolInput i CROSS JOIN Channels c", ("id",))
    with pytest.raises(ValueError, match="unique"):
        publish_with_automatic_harness(
            connection,
            model,
            [ToolOutput((7,), {"message": "ambiguous", "sent": True, "contributors": 2})],
        )
    assert connection.fetchall("SELECT * FROM ToolOutput") == []


def test_legacy_publication_rejection_leaves_automatic_connection_usable(automatic_harness):
    connection = automatic_harness
    model, keys = automatic_model(False)
    outputs = [ToolOutput(keys[0], {"message": "ok", "sent": True, "contributors": 2})]
    with pytest.raises(ValueError, match="bind_intermediates"):
        model.insert_outputs(connection, "ToolOutput", outputs)
    assert connection.fetchall("SELECT * FROM ToolOutput") == []
    publish_with_automatic_harness(connection, model, outputs)
    assert connection.fetchall(
        "INSERT INTO Final SELECT MAX(contributors), BOOL_AND(sent) FROM ToolOutput"
    ) == [(1,)]
    assert connection.fetchall("SELECT * FROM Final") == [(2, True)]
