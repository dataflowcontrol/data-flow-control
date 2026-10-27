from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .policy import Policy


@dataclass(frozen=True)
class RetryFailure:
    """One actually failing policy, identified by its stable registration index."""

    policy_index: int
    message: str
    policy: Policy | None = None


class RetryViolation(Exception):
    """Native RETRY failures, ordered by stable policy registration index.

    Messages are preserved exactly and deduplication is by policy identity, not
    message text. Singular compatibility attributes are None for multiple
    failures. This ordering makes no promise about SQL evaluation order.
    """

    def __init__(self, failures: Iterable[RetryFailure]):
        by_index = {failure.policy_index: failure for failure in failures}
        if not by_index:
            raise ValueError("RetryViolation requires at least one failing policy")
        self.failures = tuple(by_index[index] for index in sorted(by_index))
        self.messages = [failure.message for failure in self.failures]
        singleton = self.failures[0] if len(self.failures) == 1 else None
        self.policy_index = singleton.policy_index if singleton else None
        self.message = singleton.message if singleton else None
        self.policy = singleton.policy if singleton else None
        super().__init__(
            self.message if singleton else f"{len(self.failures)} RETRY policies failed"
        )


class _NativeRetrySignal(Exception):
    """Adapter-private callback evidence; never inferred from backend text."""

    def __init__(self, failures: tuple[tuple[int, str | None], ...]):
        super().__init__("Native RETRY policy signal")
        self.failures = failures
