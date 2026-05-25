from .base import Adapter, Capabilities
from .duckdb import DuckDBAdapter
from .registry import IMPLEMENTED_DIALECTS, SUPPORTED_DIALECTS, connect, create_adapter

__all__ = [
    "Adapter",
    "Capabilities",
    "DuckDBAdapter",
    "IMPLEMENTED_DIALECTS",
    "SUPPORTED_DIALECTS",
    "connect",
    "create_adapter",
]
