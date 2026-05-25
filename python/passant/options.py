from __future__ import annotations

from dataclasses import dataclass


@dataclass
class RewriteOptions:
    use_partial_push: bool = False
    collect_stats: bool = False
    dialect: str | None = None
