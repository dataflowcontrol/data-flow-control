# Installed-package smoke tests

The smoke test exercises REMOVE filtering, passing and failing KILL queries,
and INSERT enforcement including verifying that a failed INSERT writes no rows.
Run with `python -I` to prevent the checkout or `PYTHONPATH` from supplying imports.

## Test a built wheel before release

From the repository root:

```bash
uv run --extra dev maturin build --locked --out dist
uv venv /tmp/dfc-wheel-test
uv pip install --python /tmp/dfc-wheel-test/bin/python dist/*.whl
uv pip check --python /tmp/dfc-wheel-test/bin/python
/tmp/dfc-wheel-test/bin/python -I install_testing/smoke_test.py
```

Use an output directory containing only the wheel for the current platform.
On Windows, the virtual environment's Python is under `Scripts/python.exe`.

The release workflow runs this against the produced Linux x86_64, Windows x64,
and macOS ARM64/x86_64 wheels before publishing. Linux covers Python 3.10 and
3.14. Other cross-compiled architectures and musllinux are built but are not yet
execution-tested by that workflow.

## Test the published package

```bash
cd install_testing
uv sync
uv run python -I smoke_test.py
```

The base installation needs neither LangChain nor AWS packages. To exercise the
optional integration, install `data-flow-control[langchain]` separately.
