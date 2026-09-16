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

# D9: the gate used to be entirely DEBUG. `cargo clippy` and `cargo test` both
# build debug, so the RELEASE binary - the one `ship.sh` actually sends to
# meridian - was compiled for the first time at ship time, after the gate had
# already said green. That is not hypothetical: `main.rs` has real
# `#[cfg(debug_assertions)]` behaviour (the fake model port), so the two builds
# are genuinely different programs, and on 2026-09-15 a release-only warning
# reached a ship log that no gate run could have seen. A release-only COMPILE
# error would have passed the gate and failed the deploy.
#
# Only the server: it is what ships as a binary. The client's release bundle is
# built by `build-client.sh`/`build-desktop.sh`, which the gate does not cover
# either - see DEFERRED F19b, still open.
cargo build --release -p server
