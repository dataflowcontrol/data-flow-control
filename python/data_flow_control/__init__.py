from ._passant import PassantRewriteError
from .connection import dfc
from .dialect import Dialect
from .options import RewriteOptions, UiUpdateMode
from .policy import Policy, Resolution
from .ui import UiViolationEvent

__all__ = [
    "create_agent",
    "dfc",
    "Dialect",
    "langchain_dfc",
    "Policy",
    "Resolution",
    "RewriteOptions",
    "UiUpdateMode",
    "PassantRewriteError",
    "UiViolationEvent",
]


def __getattr__(name: str):
    if name == "create_agent":
        from .langchain import create_agent as _create_agent

        return _create_agent
    if name == "langchain_dfc":
        from .langchain import langchain_dfc as _langchain_dfc

        return _langchain_dfc
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
