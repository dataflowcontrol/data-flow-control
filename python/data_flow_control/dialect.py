from __future__ import annotations

from enum import Enum


class Dialect(str, Enum):
    """SQL dialects with a registered Passant adapter."""

    DUCKDB = "duckdb"
    SQLITE = "sqlite"
    POSTGRES = "postgres"
    CLICKHOUSE = "clickhouse"
    DATAFUSION = "datafusion"
    UMBRA = "umbra"

    @classmethod
    def normalize(cls, dialect: str) -> str:
        normalized = dialect.strip().lower()
        if normalized == "postgresql":
            return "postgres"
        return normalized

    @classmethod
    def contains(cls, value: str) -> bool:
        return cls.normalize(value) in cls._value2member_map_

    @classmethod
    def parse(cls, dialect: str) -> Dialect:
        normalized = cls.normalize(dialect)
        try:
            return cls(normalized)
        except ValueError:
            raise ValueError(f"Unknown dialect: {dialect!r}") from None

    @classmethod
    def supported_names(cls) -> tuple[str, ...]:
        return tuple(sorted(member.value for member in cls))
