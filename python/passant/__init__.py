try:
    from ._passant import PassantRewriteError
except ImportError:  # pragma: no cover
    PassantRewriteError = None  # type: ignore[misc, assignment]

from .adapters import IMPLEMENTED_DIALECTS, SUPPORTED_DIALECTS
from .connection import Connection, connect, wrap
from .options import RewriteOptions
from .planner import Planner
from .policy import AggregatePolicy, PgnPolicy, Policy, Resolution

__all__ = [
    "AggregatePolicy",
    "Connection",
    "IMPLEMENTED_DIALECTS",
    "PassantRewriteError",
    "Planner",
    "PgnPolicy",
    "Policy",
    "Resolution",
    "RewriteOptions",
    "SUPPORTED_DIALECTS",
    "connect",
    "wrap",
]
