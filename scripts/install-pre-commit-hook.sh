#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"

if command -v uv >/dev/null 2>&1 && uv run pre-commit --version >/dev/null 2>&1; then
  precommit=(uv run pre-commit)
elif command -v pre-commit >/dev/null 2>&1; then
  precommit=(pre-commit)
else
  echo "Installing pre-commit..."
  if command -v uv >/dev/null 2>&1; then
    uv tool install pre-commit
  else
    python3 -m pip install --user pre-commit
    export PATH="${HOME}/.local/bin:${PATH}"
  fi
  precommit=(pre-commit)
fi

"${precommit[@]}" install --hook-type pre-commit
echo "Git pre-commit hook installed. Formatting runs automatically before each commit."
