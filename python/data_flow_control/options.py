from __future__ import annotations

from dataclasses import dataclass
from enum import Enum


class UiUpdateMode(Enum):
    APPROVAL_ONLY = "approval_only"
    EDITED_ROWS = "edited_rows"


@dataclass
class RewriteOptions:
    use_partial_push: bool = False
    collect_stats: bool = False
    dialect: str | None = None
    ui_stream_endpoint: str | None = None
    ui_update_mode: UiUpdateMode = UiUpdateMode.APPROVAL_ONLY
