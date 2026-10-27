from ._passant import PassantRewriteError
from .connection import dfc
from .dialect import Dialect
from .errors import RetryFailure, RetryViolation
from .options import RewriteOptions, UiUpdateMode
from .policy import Policy, Resolution
from .ui import UiViolationEvent

__all__ = [
    "Dialect",
    "PassantRewriteError",
    "Policy",
    "Resolution",
    "RetryFailure",
    "RetryViolation",
    "RewriteOptions",
    "UiUpdateMode",
    "UiViolationEvent",
    "create_agent",
    "dfc",
    "langchain_dfc",
]


def __getattr__(name: str):
    if name == "create_agent":
        from .langchain import create_agent as _create_agent

        return _create_agent
    if name == "langchain_dfc":
        from .langchain import langchain_dfc as _langchain_dfc

        return _langchain_dfc
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
