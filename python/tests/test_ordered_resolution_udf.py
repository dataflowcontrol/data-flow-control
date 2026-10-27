"""Select repair policies once; execute functions in definition order."""

import duckdb
import pytest
from data_flow_control import Policy, Resolution, RetryViolation, dfc


@pytest.mark.parametrize("reverse_policies", [False, True])
@pytest.mark.parametrize("second_passes", [False, True])
def test_tuple_repairs_use_original_selection_and_definition_order(reverse_policies, second_passes):
    raw = duckdb.connect()
    raw.execute(
        "CREATE TABLE source(x INTEGER); INSERT INTO source VALUES (0); CREATE TABLE target(x INTEGER)"
    )
    conn = dfc(raw)
    calls = []

    def add_one(value):
        calls.append(("add", value))
        return value + 1

    def double(value):
        calls.append(("double", value))
        return value * 2

    try:
        conn.register_resolution_function(
            "add_one", add_one, ["INTEGER"], "INTEGER", splittable=False
        )
        conn.register_resolution_function(
            "double", double, ["INTEGER"], "INTEGER", splittable=False
        )
        policies = [
            ("add_one", "source.x > 0"),
            ("double", "source.x = 0" if second_passes else "source.x > 0"),
        ]
        if reverse_policies:
            policies.reverse()
        for name, constraint in policies:
            conn.register_policy(
                Policy(
                    sources=["source"],
                    sink="target",
                    constraint=constraint,
                    on_fail=Resolution.UDF,
                    udf_name=name,
                )
            )
        conn.execute("INSERT INTO target SELECT x FROM source")
        assert raw.execute("SELECT x FROM target").fetchall() == [(1 if second_passes else 2,)]
        assert calls == ([("add", 0)] if second_passes else [("add", 0), ("double", 1)])
    finally:
        conn.close()


def test_higher_retry_aborts_before_any_ordered_repair_effect():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE source(x INTEGER); INSERT INTO source VALUES (0)")
    conn = dfc(raw)
    calls = []

    def repair(value):
        calls.append(value)
        return value + 1

    try:
        for name in ["first_repair", "second_repair"]:
            conn.register_resolution_function(
                name, repair, ["INTEGER"], "INTEGER", splittable=False
            )
            conn.register_policy(
                Policy(
                    sources=["source"],
                    constraint="source.x > 0",
                    on_fail=Resolution.UDF,
                    udf_name=name,
                )
            )
        conn.register_policy(
            Policy(
                sources=["source"],
                constraint="source.x > 0",
                on_fail=Resolution.RETRY,
                retry_message="stop first",
            )
        )
        with pytest.raises(RetryViolation, match="stop first"):
            conn.execute("SELECT x FROM source")
        assert calls == []
    finally:
        conn.close()


@pytest.mark.parametrize("relation_first", [False, True])
@pytest.mark.parametrize("relation_originally_passes", [False, True])
def test_mixed_relation_tuple_order_preserves_original_relation_signal(
    relation_first, relation_originally_passes
):
    raw = duckdb.connect()
    raw.execute("CREATE TABLE source(x INTEGER); INSERT INTO source VALUES (0)")
    conn = dfc(raw)
    calls = []

    def repair(value):
        calls.append(("repair", value))
        return value + 1

    def gate(violation):
        calls.append(("gate", violation))
        return True

    try:
        definitions = ["gate", "repair"] if relation_first else ["repair", "gate"]
        for name in definitions:
            if name == "gate":
                conn.register_relation_resolution_function(name, gate, splittable=False)
            else:
                conn.register_resolution_function(
                    name, repair, ["INTEGER"], "INTEGER", splittable=False
                )
        # Deliberately reverse the function-definition order for policy insertion.
        for name in reversed(definitions):
            conn.register_policy(
                Policy(
                    sources=["source"],
                    constraint="source.x = 0"
                    if name == "gate" and relation_originally_passes
                    else "source.x > 0",
                    on_fail=Resolution.RELATION_UDF if name == "gate" else Resolution.UDF,
                    udf_name=name,
                )
            )
        assert conn.execute("SELECT x FROM source").fetchall() == [(1,)]
        effects = {"repair": ("repair", 0), "gate": ("gate", not relation_originally_passes)}
        # Legacy relation protocol includes a false callback; that is not a
        # newly selected repair and must not be confused with predicate reruns.
        assert calls == [effects[name] for name in definitions]
    finally:
        conn.close()


def test_tuple_dropping_every_row_preserves_legacy_empty_relation_callback_skip():
    raw = duckdb.connect()
    raw.execute("CREATE TABLE source(x INTEGER); INSERT INTO source VALUES (0)")
    conn = dfc(raw)
    calls = []

    def drop(value):
        calls.append(("drop", value))
        return None

    def gate(violation):
        calls.append(("gate", violation))
        return True

    try:
        conn.register_resolution_function(
            "drop_row", drop, ["INTEGER"], "INTEGER", splittable=False
        )
        conn.register_relation_resolution_function("gate", gate, splittable=False)
        for name, resolution in [("gate", Resolution.RELATION_UDF), ("drop_row", Resolution.UDF)]:
            conn.register_policy(
                Policy(
                    sources=["source"], constraint="source.x > 0", on_fail=resolution, udf_name=name
                )
            )
        assert conn.execute("SELECT x FROM source").fetchall() == []
        # Original relation failure is fixed, but its input relation is empty
        # after the drop. Keep the existing empty-input callback protocol.
        assert calls == [("drop", 0)]
    finally:
        conn.close()
