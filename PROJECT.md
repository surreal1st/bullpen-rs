# bullpen-rs

**A self-hosted AI agent platform Josh runs on his own hardware, because the
work it does involves client, employer and financial data he will not put into
Grok Bot or Cursor.**

Written 2026-09-18. Every number in the "Current state" section was measured,
not recalled. If you are an agent picking this up cold, read this file, then
`.scratch/bullpen-rs/HANDOFF.md` for the live cursor, and nothing else until
you need it.

---

## 1. What it is

A roster of named bots. You talk to one in a thread, or put several in a room
and they take turns. Each bot has its own persistent Linux machine with a
browser, its own memory, its own permissions, and its own model.

The thing that makes it worth building rather than buying: **every model call
goes through Josh's own OpenRouter key, under controls he sets** — per-bot
model pins, an escalation ladder a bot climbs when it is out of its depth, a
routing classifier that picks a cheap model for cheap work, a hard cheap floor
for anything running unattended, spend ceilings, and a permission set that
automatically tightens when he is not watching.

It is a full Rust rewrite of an earlier TypeScript product ("Bullpen", still
live and untouched). The rewrite target was set by Josh on 2026-09-14:

> *"I'm not satisfied with Bullpen. Grok Bot fits all of my needs except for
> the model controls. I believe you can get this done."*

So the goal is **Grok Bot's shape with Bullpen's model controls** — not
"Bullpen again in Rust". Where the two products differ, Grok Bot's behaviour
wins, unless the difference *is* a model control, in which case Bullpen's
wins and ports exactly, tests and all.

---

## 2. Current state (measured 2026-09-20)

| | |
|---|---|
| HEAD | `b49bdfe` on `main`, tree clean |
| Live at | `https://meridian.tail74afb5.ts.net:8452` |
| Gate | **1,293 tests**, exit 0 |
| Code | **112,422 lines of Rust** across 5 crates, 258 commits since 2026-09-14 |
| Deployed | meridian, `bullpen-rs.service` on :4380, binary hash-verified on both ends |
| Tools | 25 registered |
| API | 80 routes |
| Schema | 23 migrations, byte-compatible with the TS product's `bullpen.db` |

Crate sizes: `server` 71,985 · `client` 25,469 · `store` 9,558 ·
`model` 4,434 · `shared` 976.

**It runs on a copy of the live database, beside the live TS product, and Josh
uses it.** It has not replaced anything yet.

### What works end to end

- Threads, rooms (up to 6 bots), the round engine, SSE streaming.
- The full model-control pillar (section 4).
- Memory: profile/log/note tiers, project memory, bot-writable shared memory,
  `search_memory`.
- Auto Review — a second cheap model judges risky calls before they run.
- Routines, goals, schedules, Slack.
- Per-bot VMs: a real Linux desktop per bot, hibernated when idle, woken on
  demand, viewable in the app.
- **Computer use, including past the browser** — see section 5.

---

## 3. Architecture

```
crates/
  shared/   types both halves use — phrasing, faces, mentions
  model/    OpenRouter port, ladder, routing, floors, spend.  THE PILLAR
  store/    rusqlite schema + queries.  No HTTP, no model calls
  server/   axum app, run manager, tools, prompt, permissions, scheduler
  client/   Dioxus app (web + desktop + mobile targets)
```

**Stack:** Axum 0.8 + tokio, rusqlite (bundled), Dioxus 0.7 (webview desktop),
reqwest streaming to OpenRouter with a hand-rolled SSE parser, Docker for
sandboxes and per-bot VMs, edition 2024.

**The test seam is `build_app(state) -> Router`.** Every integration test
drives the real HTTP API through it, never internals. 65 test files under
`crates/server/tests/`.

**Database.** The same SQLite file and schema as the TS product, so a cutover
is a file copy. Migrations 1..16 are byte-for-byte equivalent; new work adds
17+. Do not renumber, do not "tidy" the old ones.

**Client.** One Dioxus codebase for desktop (webview), web (wasm), and mobile.
The desktop build signs in Rust-side and carries a Bearer token on every
request — it has no session cookie, which is a trap worth knowing (section 7).

---

## 4. The pillar: model controls

This is the part that must never regress. It is the entire reason the product
exists rather than being replaced by something off the shelf.

- **Per-bot pins** — a bot can be pinned to a specific model.
- **The ladder** — a bot that is out of its depth calls `escalate` and climbs
  cheap → mid → premium. It cannot climb past the top rung.
- **Routing classifier** — picks a cheap model for cheap work, with a log and
  a settings panel.
- **Floors** — `modelForRun` enforces a hard cheap floor for unattended runs,
  regardless of what anything else asked for.
- **Spend ceilings** — checked against the account's real OpenRouter balance,
  not a local guess.
- **`TIGHTEN`** — the permission set narrows automatically when a run is
  unattended. A tool that is `Allow` while Josh is watching a chat becomes
  `Ask` when nobody is.

`TIGHTEN` is the key idea and it recurs: **the approval that means something is
the unattended one.** An approval prompt in front of every keystroke is not a
decision Josh is making, it is a prompt he clears to get the thing he already
asked for.

---

## 5. Computer use, and what it can and cannot do

A bot drives its **own** VM. Never a shared one — that boundary is the point,
because one bot running an errand on a machine holding every other bot's
cookies is exactly the failure this design exists to prevent.

**Two layers, and the difference matters:**

| Layer | Tools | Reaches |
|---|---|---|
| Browser, via Chrome DevTools | `browse`, `read_page`, `click`, `type_text` | only what Chromium can see, by reading the page |
| The whole screen, via `xdotool` | `desk_act` | any window at all — a terminal, a file manager, a native dialog |
| The machine, via a shell | `desk_shell` | the container's filesystem and network |

**`desk_act` was half-blind; it is not any more, and section 9a has the
accepted result.** Keyboard actions (`key`, `type`, `wait`) work — proven on a
real VM (`shots/s8c-03-desk-act-typed-example-com.png`). **Mouse** actions
(`click`, `move`, `drag`, `scroll`) now take coordinates a bot derives from its
own `snap_desk`, and `desk_act` REFUSES a mouse action carrying no
`observation_id`.

🔴 **What is unsolved is AIMING, not plumbing.** The mouse physically acts
(proven 2026-09-19 by a pointer move and a window restack), but no model in
the roster could reliably hit a target — `gemini-3.8-flash` missed a button by
123px in y — and **both models then asserted a success they had not
achieved.** A bot's own account of what it did is never evidence. See
`.scratch/bullpen-rs/reviews/S8d-native-gui-acceptance-RESULT.md`.

**Security properties that hold this together.** If you touch this area, these
are the ones to not break:

1. **Typed text goes over stdin, never into a command string.** The text
   reaches `bash -lc` inside the container; interpolating it would make every
   character a bot types a shell command. `xdotool type --file -` with the text
   piped.
2. **`keys` is interpolated, and is safe only because its pattern is anchored
   at both ends.** Rust's `Regex::is_match` is a *search*, not a full match —
   an unanchored port silently accepts `a; rm -rf /`. This is tested by
   mutation; the unanchored version demonstrably builds
   `bash -lc "DISPLAY=:1 xdotool key a; touch /tmp/pwned"`.
3. **Every URL is re-validated on every hop**, including after redirects. An
   earlier version fenced only the first hop, so a 302 to a link-local address
   walked straight through.
4. **Every tool result that carries machine- or web-derived text is fenced** as
   data before it reaches a prompt (`fence_tool_output`). A page that says
   "ignore your previous instructions" is a finding to report, never a command
   to run. Server-generated text (refusals, numbered prefixes) stays unfenced,
   deliberately.
5. **Secrets never reach a `Debug` impl.** PEM keys once printed through one.

---

## 6. The build spec

### Prerequisites

- Rust (rustup), edition 2024 toolchain — see `rust-toolchain.toml`.
- `zig` (via winget) for cross-compilation.
- Target `x86_64-unknown-linux-musl` (`ship.sh` adds it if missing).
- Node, for the screenshot and smoke scripts.
- The OpenRouter key at `C:\Users\rain\.bullpen\openrouter.key`. Never on a
  command line, never in a workspace file.

### The gate — this is what "done" means

```bash
bash scripts/gate.sh          # run from a CLEAN worktree
echo "GATE EXIT=$?"           # the EXIT CODE is the verdict
```

It runs, in order: `cargo fmt --all --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --all-features --no-fail-fast`,
`cargo build --release -p server`, and `cargo check -p client --target
wasm32-unknown-unknown` (default features — `default = ["web"]` *is* the
browser configuration; turning desktop and mobile on under wasm32 would check a
target that does not exist).

Three things about it that were each learned the hard way and are written into
the script's own comments:

- `--no-fail-fast`, because one red suite used to hide every suite after it.
- The **release** build is in the gate, because `main.rs` has real
  `#[cfg(debug_assertions)]` behaviour — debug and release are genuinely
  different programs, and a release-only compile error would otherwise pass the
  gate and fail the deploy.
- The **wasm** check is in the gate, because every other line runs on the
  native host and nothing used to compile the browser client at all.

🔴 **Never pipe the gate through `grep`/`tail`/`head`.** The pipeline's exit
status replaces the gate's, so a red suite reports success. Redirect to a file
and grep the file afterwards.

### Shipping

```bash
bash scripts/ship.sh
# then, on meridian, as printed by the script:
sudo bash /home/rainmade/bullpen-rs-incoming/<timestamp>/install.sh
```

**The build happens on the workstation; meridian only runs what it is handed.**
An uncapped build on that box once saturated it and took every service down.
The transfer is sha256-verified on both ends, because a silent transcode has
corrupted a file move between these two machines before and a copy command's
exit code would not have caught it.

After installing, verify the running service *is* your commit by hash, not by
assumption:

```bash
ssh meridian "sudo sha256sum /home/bullpen/bullpen-rs/bullpen"
# compare against the "Binary SHA256:" line ship.sh printed
```

### Configuration

Set in `deploy/bullpen-rs.service`. Two that matter most:

- `BULLPEN_VM=on` — **this is what makes computer use exist.** It defaults OFF,
  and with it unset every desk tool refuses with "Per-bot machines are off
  here" while the gate stays green, because nothing in the test suite reads the
  real env var.
- `RUST_LOG=info,server=debug,model=debug,bullpen=debug` — without it the
  server is completely silent, because `tracing` emits nothing by default. A
  server that discards its own evidence cannot be diagnosed, only guessed at.

🔴 `systemctl show -p Environment` does **not** list `EnvironmentFile`
contents. To see what a running service actually has, read
`/proc/<MainPID>/environ` — and print variable **names** only, never values.

---

## 7. Rules that bind every agent working here

These are not style preferences. Each one is a scar.

- **Never build on meridian.** Cross-compile here, ship the binary.
- **Never touch live Bullpen** — `bullpen.service`, port :4360. It is a
  different, running product. bullpen-rs is :4380.
- **Run git only from `projects/bullpen-rs`, never from the gate worktree at
  `projects/bullpen-rs-gate`.** A commit there lands on a detached HEAD. This
  has eaten a commit twice.
- **Never `git add -A`, never a bare `git commit`.** Builders share one working
  tree, so either sweeps in whatever another builder has staged. Name every
  file: `git commit -m "..." -- <file> <file>`.
- **No recursive deletes under `d:\rainmade`.** It is not backed up and Git
  Bash deletes skip the Recycle Bin.
- **No secrets on command lines or in workspace files.**
- **Prove every test bites.** Break what it guards, watch it go red, restore,
  confirm the restore is byte-identical, watch it go green. A mutation that
  stays green is a defective test until proven otherwise — five out of five
  were.
- **Restore mutations in BINARY mode.** Python's text mode on Windows rewrites
  every line ending; the result is an empty `git diff` against a dirty
  `git status`. Check `git status --porcelain` after every restore.
- **`bullpen-desk` is live Bullpen's machine, not an orphan.** It binds
  :9223/:6101, which are exactly `desk::desk_config`'s hardcoded defaults, and
  the overriding env vars are unset on the box. Never let new code reach
  `desk_config(env)` — a per-bot `DeskConfig` comes from
  `vm::vm_desk(&row, &cfg)`, only.
- **`.scratch/bullpen-rs/tickets/DEFERRED.md` rots by the day.** Ten entries
  were stale on 09-17; four more went stale within the next twenty hours. Read
  the code before believing anything in it.

### Verification posture

- **"(done)", "exit 0" and "healthy" prove nothing.** `desk_act` returns
  `1. (done)` whether or not `xdotool` did anything. A check that shares its
  target's blind spot returns green and certifies the bug.
- **A green gate cannot prove a log line appears.** Instrumentation once
  shipped that never fired; only reading the journal caught it.
- **Verify from a different angle than the implementation.** For anything
  touching a screen, that means taking a screenshot and opening it.
- **Check which function is actually dispatched before writing a ticket.** Two
  tickets in one session named code that did not do what they claimed; both
  builders caught it. Read the code, not the last handoff's summary of it.

---

## 8. What is left (2026-09-22)

Measured against TS **bullpen-night** (229 route registrations, 63 bot tools).
**bullpen-rs @ `main`** exposes ~**150** HTTP path patterns and **~60+** dispatched
tools (see `.scratch/bullpen-rs/INVENTORY.md` and
`reviews/PARITY-2026-09-22-RESULT.md`). S7–S12 server slices are largely on
meridian **:4380**; the finish line is **cutover + waivers**, not greenfield
porting.

**Shipped on :4380 (internal Grok Bot baseline):** chat/runs/rooms, model
controls, memory + shared/projects, approvals/questions, routines/goals/hooks,
connectors + OAuth, jobs/repo/spawn_helper, skills/marketplace/import,
users/invites, Slack (not Telegram), push register API, attachments/library,
media tools (deliver, draw, transcribe, voice/video review), purchasing/Stripe
webhook, databases/workers, desk/VM/computer use, desktop + web client.

**Cutover (S14 — Josh):** production DB copy, rendered smoke, nginx flip
`:4360` → `:4380`, 24h beside TS, then TS decommission per `docs/s14-cutover.md`.
Automated: `scripts/cutover-check.sh`.

**Parity waivers (approved for internal finish line):** Telegram notify, Teams
stub, native iOS build/sign (API present).

**Known HTTP gaps (mostly post-cutover OK unless Josh promotes):** vault/history
REST, run rewind/undo/snapshot, message reactions, global `/api/search`, demo/teach
routes, `GET /api/bots` (roster covers UI), Bullpen-as-MCP HTTP `/mcp`, report
cards, a few TS admin/diagnostic paths. Details in the parity review.

**Optional / polish:** S13 iOS ship, UI gaps vs TS CSS surface, SEC5-02 observation
history if repro appears, F19b desktop client release compile in gate.

---

## 9. The spec for the next work

### 9a. "A bot sees its own screen" — BUILT, and accepted 2026-09-19

**Decided by Josh, 2026-09-17:** a bot should get its own screenshot back.
Video stays out for now.

🔴 **This section described this as unbuilt design work until 2026-09-19. That
was stale and cost nothing only because it was caught.** The image path EXISTS:
`snap_desk` is registered and dispatched (`tools/snap_desk.rs`,
`tools/mod.rs`), and `runs::screen_observation_request_message`
(`runs.rs:431`) builds a real `MessageContent::Parts` carrying a base64 PNG
plus provenance text that fences visible screen text as untrusted. The design
question below was answered IN CODE: the image arrives as a **following user
message**, not as a tool result.

**What the acceptance actually proved** — see
`.scratch/bullpen-rs/reviews/S8d-native-gui-acceptance-RESULT.md` for the
evidence, the cost and the reproduction:

- A bot READ a random token off its own screen with every other tool denied.
- `desk_act`'s mouse physically acts (pointer moved; window stacking changed),
  and it correctly REFUSES a click that carries no `observation_id`.
- **Neither model could reliably hit a target** — `gemini-3.8-flash` missed a
  button by 123px in y — and both then asserted a success they had not
  achieved. Aiming, not plumbing, is what is unsolved.
- 🔴 `google/gemini-2.5-flash-lite` returns `MALFORMED_FUNCTION_CALL`
  deterministically on the SECOND screenshot in history. Since `modelForRun`
  holds a hard cheap floor for unattended runs, an unattended computer-use
  loop is pushed onto exactly the model that cannot sustain one.

**The original design questions, kept because two are still open:**

- Does the screenshot come back as the **tool result**? Tool messages are
  text-only in the OpenAI/OpenRouter shape, and support for an image there is
  not universal.
- Or as a **following user message** carrying an image part, which is how most
  computer-use loops do it, at the cost of a synthetic turn in the transcript?
- Which models in the roster actually accept an image, and what happens to the
  ones that do not? (`model/src/port.rs:238-240` already detects that a message
  carries an image — start there.)
- What does it cost per screenshot, and does the cheap floor still hold for an
  unattended run that is taking pictures in a loop?

**The rejected shortcut, still rejected:** writing a PNG into the VM and
telling the model to open it with `review_media` points the model at a tool
that is a permission row with **no implementation**. `snap_desk` was built
properly instead — it returns the observation to the run, which turns it into
an image message.

**What remains open, and both are Josh's calls:**

1. ~~May the cheap floor select a model for a run that carries screen
   observations?~~ **Settled 2026-09-20 (SEC5-01):** unattended runs (and
   room rounds) lift off flash-lite to tier-1 vision when the provider call
   carries images; Josh-initiated chat keeps his pin.
2. Should history carry more than the latest observation? Every extra
   screenshot is paid for on every later step of the same run.

### 9b. `tool_choice`, with a per-model capability flag

**Decided by Josh, 2026-09-17.** Insurance, not a blocker — computer use works
without it.

Measured behaviour, which is the whole reason it needs a flag:

| Model | `tool_choice: required` |
|---|---|
| `gemini-3.8-flash` | obeys — called a tool it had ignored on an adversarial prompt |
| `qwen3.8-flash` | HTTP 400 |
| `gpt-oss-120b` | ignores the force |

So it cannot be a global setting. It needs a per-model capability flag, a
caller that wants it, and a fallback path for the models that 400.

### 9c. Smaller open items

- **S8b-F1 — runs that finish with no text.** 🔴 **A cause is now PROVEN for
  one member of this class** (2026-09-19): the provider returns a choice-level
  `finish_reason: "error"` carrying `native_finish_reason:
  "MALFORMED_FUNCTION_CALL"`, and `port.rs`'s `FrameChoice` models neither
  field, so serde discards the diagnosis and the operator sees "The model
  provider completed without an answer." Reproduced 3/3 with
  `gemini-2.5-flash-lite` and two images in history. **This does not establish
  that the historical ~9% — measured on a chat mix with no images — had the
  same root cause.** Look for `model stream produced no text and no tool call`
  in `journalctl -u bullpen-rs`.
- **`message_bot`'s `ModelEvent::Error` branch is unfenced.** Probably correct —
  that text is the model port's error surface, not a bot's answer — but nobody
  has decided it deliberately.
- **`vm.rs:183` interpolates a raw `bot_id`** into a volume name while
  `sandbox.rs:86 volume_for` sanitizes. Hygiene, not a vulnerability: there is
  no bot-creation route (`routes/bots.rs` is PATCH-only) and every real id is a
  server-assigned lowercase slug.

---

## 10. Where things live

| | |
|---|---|
| Repo | `d:\rainmade\projects\bullpen-rs` · GitHub `surreal1st/bullpen-rs` (PRIVATE) |
| Gate worktree | `d:\rainmade\projects\bullpen-rs-gate` (never run git here) |
| Handoff (in repo) | `docs/CLAUDE-HANDOFF-4.md` — the full handoff, start here |
| Cursor (in repo) | `docs/CURRENT-CURSOR.md` — a COPY of `HANDOFF.md`'s first block |
| Live cursor | `.scratch/bullpen-rs/HANDOFF.md` — read the top block first |
| Plan | `.scratch/bullpen-rs/PLAN.md` — slices, rationale |
| Parity checklist | `.scratch/bullpen-rs/INVENTORY.md` |
| Tickets | `.scratch/bullpen-rs/tickets/` |
| Deferred findings | `.scratch/bullpen-rs/tickets/DEFERRED.md` — rots by the day |
| Designs | `.scratch/bullpen-rs/designs/` |
| Screenshots | `shots/` (gitignored) |
| TS original | `projects/bullpen-night` — read-only reference, LIVE product, never edit |

**Working method.** Tickets are written by an orchestrator and built one
builder at a time — every new tool touches `tools/mod.rs`, so parallel builders
deadlock on the pre-commit hook. The orchestrator re-runs every bite by hand
and runs the gate itself; a pasted red from a builder is not evidence. Each
slice ends in something Josh can open.
