#!/usr/bin/env bash
# Optional CI helper: run Criterion smoke benchmarks and fast-path regression tests.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== Fast-path regression tests =="
cargo test -p passant-core --test rewrite_fast_paths --test rewrite_candidate_regression

echo ""
echo "== Criterion smoke (rewrite_no_policies, rewrite_one_candidate, rewrite_no_candidates) =="
for group in rewrite_no_policies rewrite_one_candidate rewrite_no_candidates; do
  echo "--- $group ---"
  cargo bench -p passant-core --bench rewrite_perf -- "$group" --sample-size 10 2>&1 | tail -8
done

echo ""
echo "Perf budget smoke complete."
echo "Documented budgets and full matrix: developer-docs/performance.md"
