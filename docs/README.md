# Passant Architecture Docs

Passant is a Rust-first SQL policy rewriter with a thin Python API for DuckDB execution.

## Documents

| Doc | Contents |
|-----|----------|
| [rewrite-pipeline.md](rewrite-pipeline.md) | Parse → validate → analyze → strategy → rewrite → format |
| [policy-validation.md](policy-validation.md) | Syntax vs catalog validation, normalization, errors |
| [strategy-selection.md](strategy-selection.md) | Full-Push, Partial-Push, source-sets, explain fields |
| [python-api.md](python-api.md) | Python `wrap` / `Connection` / `Planner` / adapters |

## Quality plan status

The structural quality plan (`passant-quality-improvement-final.md` at repo root) is **complete**:

- **CI** — `.github/workflows/passant.yml` runs `cargo fmt`, `clippy`, `cargo test`, `maturin develop`, `ruff`, and `pytest` on Passant changes.
- **Rewriter split** — `rewriter/` is orchestration plus focused modules (`select`, `write_path`, `aggregates`, `policy_expr`, etc.).
- **Structured errors** — `diagnostics::RewriteError` with stable `ErrorKind`; Python raises `PassantRewriteError` (subclass of `ValueError`) with a `.kind` attribute.
- **Rust catalog validation** — `catalog.rs` with typed `TableKey` / `ColumnName` lookups.
- **AST-backed SQL** — `sql/builders.rs` and `sql/expr.rs`; internal temp names via `passant_*` helpers.
- **Semantic tests** — execution oracles, explain strategy tests, identifier stress tests in `tests/gaps/identifiers.rs`.
- **Typed identifiers** — `identifiers.rs` (`TableName`, `TableKey`, `QualifiedColumn`, `SourceName`, `SinkName`, …).
- **Python thinning** — all policies in Rust; list normalization and constraint syntax via PyO3.
- **Docs** — this index plus the four topic guides above.

## Extension points (quick reference)

- **New policy rule** — `policy.rs` parse + `catalog.rs::validate_policy` + filter builders in `rewriter/policy_expr.rs`.
- **New rewrite path** — implement `RewriteEngine`, register in `rewrite_strategy.rs`, add tests under `tests/rewrite/`.
- **New SQL helper** — prefer `sql/builders.rs`; render with `Statement::to_string()` only at boundaries.

## Local verification

```bash
cd passant
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
uv run maturin develop
uv run ruff check .
uv run pytest
```
