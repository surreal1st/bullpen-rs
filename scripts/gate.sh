#!/usr/bin/env bash
# The gate. Green here from a clean worktree is what "done" means.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
# --no-fail-fast: one red suite must not hide the suites after it (2026-09-14 the
# rooms suite failed and runs, sandbox, settings... never ran; GATE EXIT still told
# the truth, but the fix list was incomplete).
cargo test --all-features --no-fail-fast
