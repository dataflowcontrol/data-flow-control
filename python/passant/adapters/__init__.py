from .base import Adapter, Capabilities
from .duckdb import DuckDBAdapter
from .registry import create_adapter, sniff_dialect

__all__ = [
    "Adapter",
    "Capabilities",
    "DuckDBAdapter",
    "create_adapter",
    "sniff_dialect",
]
