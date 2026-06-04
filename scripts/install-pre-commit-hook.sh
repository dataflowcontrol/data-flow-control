#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

if ! command -v pre-commit >/dev/null 2>&1; then
  echo "Installing pre-commit..."
  if command -v uv >/dev/null 2>&1; then
    uv tool install pre-commit
  else
    python3 -m pip install --user pre-commit
    export PATH="${HOME}/.local/bin:${PATH}"
  fi
fi

pre-commit install --hook-type pre-commit
echo "Git pre-commit hook installed. Formatting runs automatically before each commit."
