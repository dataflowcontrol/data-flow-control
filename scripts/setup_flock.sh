#!/usr/bin/env bash
# Install and load the Flock DuckDB community extension for local optional tests.
set -euo pipefail
cd "$(dirname "$0")/.."
uv run python -c "
import duckdb
conn = duckdb.connect()
conn.execute('INSTALL flock FROM community')
conn.execute('LOAD flock')
print('Flock extension installed and loaded.')
print('Optional execution tests also need OPENAI_API_KEY or FLOCK_OPENAI_API_KEY.')
"
