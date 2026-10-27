"""Private execution-local transport for engines that wrap Python UDF errors."""

from __future__ import annotations

from contextlib import contextmanager
from threading import Lock
from weakref import WeakValueDictionary

from ..errors import _NativeRetrySignal


class RetryState:
    def __init__(self):
        self.lock = Lock()
        self.active = False
        self.failure = None

    def signal(self, policy_ids):
        if not self.active:
            raise RuntimeError("Native RETRY must execute through the DFC adapter")
        parts = policy_ids.split(",") if isinstance(policy_ids, str) else []
        if not parts or any(not p or not p.isascii() or not p.isdigit() for p in parts):
            raise RuntimeError("Invalid native RETRY policy identity list")
        self.failure = tuple((index, None) for index in sorted({int(p) for p in parts}))
        raise _NativeRetrySignal(self.failure)

    @contextmanager
    def execution(self):
        if not self.lock.acquire(blocking=False):
            raise RuntimeError("Overlapping executions on a RETRY connection are not supported")
        self.active, self.failure = True, None
        try:
            yield
        except Exception as exc:
            if self.failure is not None:
                raise _NativeRetrySignal(self.failure) from exc
            raise
        finally:
            self.active, self.failure = False, None
            self.lock.release()


# sqlite3.Connection cannot be weak-referenced. The installed callback and adapter
# own the state, not this registry; state never owns the engine. A live adapter
# owns its engine too, preventing identity reuse while this entry is reachable.
_states: WeakValueDictionary = WeakValueDictionary()
_registration_lock = Lock()


def register_retry(engine, install):
    with _registration_lock:
        key = id(engine)
        state = _states.get(key)
        if state is None:
            state = RetryState()
            install(state)
            _states[key] = state
        return state
