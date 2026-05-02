# Passant

Passant is a Rust-backed Data Flow Control rewrite engine intended to replace
`sql_rewriter` while preserving the existing Python integration surface.

## Workspace

- `passant-core`: parser, IR, planner, optimizer, and explain output.
- `passant-cli`: CLI for rewrite, explain, plan, and policy parsing.
- `passant-py`: PyO3 extension module used by the Python package.
- `python/passant`: thin Python compatibility layer.

## Current Status

This initial implementation establishes:

- `sqlparser-rs` as the parser frontend
- a Passant-owned `QueryIr`
- a heuristic rewrite optimizer with explain output
- a CLI
- a Python compatibility package skeleton

It does not yet implement full `sql_rewriter` parity.
