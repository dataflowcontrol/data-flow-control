from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol


@dataclass(frozen=True)
class Capabilities:
    """Backend features used for policy registration and rewrite strategy selection."""

    exception_udf: bool = False
    update_from: bool = True
    aggregate_filter: bool = True
    cte_in_insert: bool = True

    @property
    def supports_kill(self) -> bool:
        return self.exception_udf


class Adapter(Protocol):
    dialect: str
    capabilities: Capabilities

    def execute(self, sql: str, params: Any = None): ...

    def introspect_catalog(self) -> dict: ...

    def quote_identifier(self, name: str) -> str: ...

    def register_kill_function(self) -> None: ...

    def close(self) -> None: ...
