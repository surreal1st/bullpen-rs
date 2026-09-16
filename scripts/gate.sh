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
# Only the server: it is what ships as a binary. The client's release BUNDLE is
# still built by `build-client.sh`/`build-desktop.sh` and is not covered here;
# the browser client's compile is, by the wasm32 check below (F19b, half closed
# 2026-09-16 - the DESKTOP client remains uncovered).
cargo build --release -p server

# F19b: the gate's compile coverage of the BROWSER client was ZERO. Every cargo
# line above runs on the native host, and `--all-features` turns web+desktop+
# mobile on together - a combination nothing ships. After S13a-01b split every
# client file on `target_arch`, nothing in this script ever compiled the wasm
# half, so a change could break the shipped browser client and still go green.
#
# DEFAULT features on purpose, not `--all-features`: `default = ["web"]`
# (`crates/client/Cargo.toml:69`) IS the browser configuration. Turning desktop
# and mobile on under wasm32 would check a target that does not exist.
#
# `check`, not `build`: this catches type and cfg errors, which is the whole
# failure class F19b describes, without paying for codegen on a third target.
# The real bundle is still built by `build-client.sh`/`build-desktop.sh`, and
# the DESKTOP client remains uncovered - that half of F19b stays open.
cargo check -p client --target wasm32-unknown-unknown
