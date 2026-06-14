#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

docker compose up -d
export PASSANT_INTEGRATION_TESTS=1
uv sync --extra dev --extra postgres --extra clickhouse --extra datafusion
uv run maturin develop -q
uv run pytest python/tests/ -v -m "postgres or clickhouse or umbra" "$@"
