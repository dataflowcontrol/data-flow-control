from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol


@dataclass(frozen=True)
class Capabilities:
    """Backend features enforced at policy registration time."""

    exception_udf: bool = False
    tuple_udf: bool = False
    relation_udf: bool = False
    ui_resolution: bool = False

    @property
    def supports_kill(self) -> bool:
        return self.exception_udf


class Adapter(Protocol):
    dialect: str
    capabilities: Capabilities

    def execute(self, sql: str, params: Any = None): ...

    def introspect_catalog(self) -> dict: ...

    def introspect_aggregate_functions(self) -> list[dict]: ...

    def quote_identifier(self, name: str) -> str: ...

    def register_kill_function(self) -> None: ...

    def register_resolution_function(
        self,
        name: str,
        func: Any,
        parameter_types: list[Any],
        return_type: Any,
    ) -> None: ...

    def register_relation_resolution_function(
        self,
        name: str,
        func: Any,
    ) -> None: ...

    def close(self) -> None: ...
