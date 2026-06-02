# passant install testing

Minimal project that installs `data-flow-control` and `duckdb` into an isolated uv environment and runs a one-file smoke test.

## After PyPI release

```bash
cd passant_install_testing
uv sync
uv run python smoke_test.py
```

## Before PyPI release (monorepo)

`uv sync` will fail until `data-flow-control` is on PyPI. Install the local package from `passant/` instead:

```bash
cd passant_install_testing
uv venv
uv pip install "duckdb>=1.3.0"
uv pip install -e ../passant
.venv/bin/python smoke_test.py
```

(`uv run` re-resolves `pyproject.toml` against PyPI and will fail until the package is published.)

The smoke test inserts `(1, 2)` into `foo`, registers `max(foo.id) > 1` with `ON FAIL REMOVE`, and checks that `SELECT id FROM foo` returns only `[(2,)]`.
