import duckdb
import pytest
from data_flow_control import dfc


def test_builtin_resolution_properties_and_conservative_udf_default():
    conn = dfc(duckdb.connect())
    try:
        native = conn.planner.inner
        assert native.resolution_splittable("KILL")
        assert native.resolution_splittable("REMOVE")
        assert not native.resolution_splittable("RETRY('message')")
        assert not native.resolution_splittable("UDF unregistered_legacy")
    finally:
        conn.close()


@pytest.mark.parametrize("splittable", [False, True])
def test_tuple_udf_registration_forwards_native_properties(splittable):
    conn = dfc(duckdb.connect())
    try:
        conn.register_resolution_function(
            "repair_value", lambda x: x, ["BIGINT"], "BIGINT", splittable=splittable
        )
        assert conn.planner.inner.resolution_splittable("UDF REPAIR_VALUE") is splittable
    finally:
        conn.close()


@pytest.mark.parametrize("splittable", [False, True])
def test_relation_udf_registration_forwards_native_properties(splittable):
    conn = dfc(duckdb.connect())
    try:
        conn.register_relation_resolution_function(
            "repair_relation", lambda violated: not violated, splittable=splittable
        )
        assert (
            conn.planner.inner.resolution_splittable("RELATION UDF repair_relation") is splittable
        )
    finally:
        conn.close()


def test_omitted_splittability_is_conservative_and_non_bool_rejected():
    conn = dfc(duckdb.connect())
    try:
        conn.register_resolution_function("repair", lambda x: x, ["BIGINT"], "BIGINT")
        assert not conn.planner.inner.resolution_splittable("UDF repair")
        with pytest.raises(TypeError, match="splittable"):
            conn.register_resolution_function(
                "invalid", lambda x: x, ["BIGINT"], "BIGINT", splittable=1
            )
        with pytest.raises(TypeError, match="splittable"):
            conn.register_relation_resolution_function("invalid", lambda x: x, splittable="yes")
    finally:
        conn.close()


def test_definition_registration_order_is_global_and_failed_definition_does_not_advance_it():
    conn = dfc(duckdb.connect())
    try:
        conn.register_resolution_function("first_repair", lambda x: x, ["BIGINT"], "BIGINT")
        first = conn.planner.inner.resolution_registration_order("first_repair")
        with pytest.raises(Exception):
            conn.register_resolution_function("first_repair", lambda x: x, ["BIGINT"], "BIGINT")
        conn.register_relation_resolution_function("second_repair", lambda failed: not failed)
        second = conn.planner.inner.resolution_registration_order("second_repair")
        assert first == 0 and second == 1
        conn.planner.register_resolution_splittability("first_repair", splittable=True)
        assert conn.planner.inner.resolution_registration_order("first_repair") == first
    finally:
        conn.close()
