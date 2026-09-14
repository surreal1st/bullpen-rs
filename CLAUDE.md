# bullpen-rs

The Rust rewrite of Bullpen: Grok Bot's shape with Bullpen's model controls.
Plan: `d:\rainmade\.scratch\bullpen-rs\PLAN.md`. Parity checklist:
`.scratch/bullpen-rs/INVENTORY.md`. Tickets: `.scratch/bullpen-rs/tickets/`.
Handoff: `.scratch/bullpen-rs/HANDOFF.md`. The TS original is read-only
reference at `projects/bullpen-night` (LIVE product, never edit it from here).

## Hard rules

- **Never build on meridian.** `cargo zigbuild --target x86_64-unknown-linux-musl`
  here, ship the binary. An uncapped build on that box once took every
  service down.
- **Never touch live Bullpen** (`bullpen.service`, :4360, `rainmade.io/bullpen/`)
  until this passes the same gate and Josh has used it. bullpen-rs runs
  BESIDE it on **:4380** (4370/4371 are taken on the workstation by
  bullpen-night preview servers; never kill those).
- **Commit with explicit paths: `git commit -m "..." -- <file> <file>`.**
  Builders share ONE working tree, so a bare `git commit` sweeps in whatever
  another builder has staged. This happened on S0-04/S0-05 (2026-09-14) and
  cost a reset. Never `git add -A`, never a bare `git commit`.
- **No secrets on command lines or in workspace files.** The OpenRouter key
  is read from `C:\Users\rain\.bullpen\openrouter.key` / `/home/bullpen/bullpen.env`.
- **No recursive deletes under `d:\rainmade`.**
- **Same SQLite schema as Bullpen.** `crates/store` carries migrations 1..16
  byte-for-byte equivalent and every self-creating table/column, so
  `bullpen.db` opens unchanged. New features add migrations 17+.
- **Same API paths as Bullpen** where the feature exists. New paths only for
  new features. The iOS client and scripts keep working.

## Layout

- `crates/shared` types both halves use (port of `src/shared`).
- `crates/model` OpenRouter port, ladder, routing, floors, spend. THE PILLAR.
- `crates/store` rusqlite schema + queries. No HTTP, no model calls.
- `crates/server` axum app builder (`build_app(state) -> Router`, the seam
  every test drives), run manager, tools, prompt, permissions, scheduler.
- `crates/client` Dioxus app (web + desktop + mobile).
- `tests/` integration tests drive the HTTP API through `build_app`, never
  internals.

## Gate

`scripts/gate.sh`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test`. Run it from a clean `git worktree` before claiming a commit is
green.

## Testing posture (unchanged from Bullpen)

- Test what a client can observe. Never assert a function was called.
- **Prove every test bites**: break what it guards, watch it go red, restore.
  A mutation that stays green is a defective test until proven otherwise.
- Model responses in tests come from captured real OpenRouter responses.
- **Look at the page.** A client slice is done when `scripts/shot` produced a
  picture and someone opened it.
