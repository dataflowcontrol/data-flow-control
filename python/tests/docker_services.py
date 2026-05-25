"""Docker-backed integration services for Passant adapter tests."""

from __future__ import annotations

import os
import shutil
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

DEFAULT_POSTGRES_URL = "postgresql://postgres:passant@127.0.0.1:15432/passant"
DEFAULT_CLICKHOUSE_URL = "clickhouse://passant:passant@127.0.0.1:18123/default"
DEFAULT_UMBRA_URL = "postgresql://postgres:postgres@127.0.0.1:15433/postgres"

COMPOSE_FILE = Path(__file__).resolve().parents[2] / "docker-compose.yml"


@dataclass(frozen=True)
class PassantDockerStack:
    postgres_url: str
    clickhouse_url: str
    umbra_url: str

    @classmethod
    def from_env(cls) -> PassantDockerStack:
        return cls(
            postgres_url=os.environ.get("PASSANT_POSTGRES_URL", DEFAULT_POSTGRES_URL),
            clickhouse_url=os.environ.get("PASSANT_CLICKHOUSE_URL", DEFAULT_CLICKHOUSE_URL),
            umbra_url=os.environ.get("PASSANT_UMBRA_URL", DEFAULT_UMBRA_URL),
        )

    @classmethod
    def start(cls, *, timeout_s: int = 120) -> PassantDockerStack:
        if shutil.which("docker") is None:
            raise RuntimeError("docker is not available on PATH")
        if not COMPOSE_FILE.is_file():
            raise RuntimeError(f"missing compose file: {COMPOSE_FILE}")

        subprocess.run(
            ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d"],
            check=True,
            capture_output=True,
            text=True,
        )
        stack = cls.from_env()
        stack.wait_ready(timeout_s=timeout_s)
        return stack

    def wait_ready(self, *, timeout_s: int = 120) -> None:
        deadline = time.time() + timeout_s
        last_error: Exception | None = None
        while time.time() < deadline:
            try:
                self._wait_postgres()
                self._wait_clickhouse()
                self._wait_umbra()
                return
            except Exception as exc:
                last_error = exc
                time.sleep(1)
        raise RuntimeError(f"Passant docker services did not become ready: {last_error}")

    def _wait_postgres(self) -> None:
        psycopg = _require_psycopg()
        with psycopg.connect(self.postgres_url, connect_timeout=3) as conn:
            conn.execute("SELECT 1")

    def _wait_clickhouse(self) -> None:
        clickhouse_connect = _require_clickhouse_connect()
        parsed = _parse_clickhouse_url(self.clickhouse_url)
        client = clickhouse_connect.get_client(**parsed)
        try:
            client.query("SELECT 1")
        finally:
            client.close()

    def _wait_umbra(self) -> None:
        psycopg = _require_psycopg()
        with psycopg.connect(self.umbra_url, connect_timeout=3) as conn:
            conn.execute("SELECT 1")


def _require_psycopg():
    try:
        import psycopg
    except ImportError as exc:  # pragma: no cover
        raise RuntimeError("psycopg is required: uv sync --extra postgres") from exc
    return psycopg


def _require_clickhouse_connect():
    try:
        import clickhouse_connect
    except ImportError as exc:  # pragma: no cover
        raise RuntimeError("clickhouse-connect is required: uv sync --extra clickhouse") from exc
    return clickhouse_connect


def _parse_clickhouse_url(url: str) -> dict[str, object]:
    from urllib.parse import unquote, urlparse

    parsed = urlparse(url)
    if parsed.scheme != "clickhouse":
        raise ValueError(f"expected clickhouse URL, got {url!r}")
    return {
        "host": parsed.hostname or "127.0.0.1",
        "port": parsed.port or 8123,
        "username": unquote(parsed.username) if parsed.username else "default",
        "password": unquote(parsed.password) if parsed.password else "",
        "database": (parsed.path or "/default").lstrip("/") or "default",
    }
