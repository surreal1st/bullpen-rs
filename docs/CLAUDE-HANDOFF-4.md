# Bullpen-rs: handoff from the 2026-09-19 Claude session

Prepared 2026-09-20 00:05 EDT. This supersedes `CLAUDE-HANDOFF-3.md`, which was
the handoff INTO this session. Everything it says about STATE is now stale; its
rules, recipes and method still hold and are carried forward below.

## Start here

1. This file, front to back.
2. `projects/bullpen-rs/PROJECT.md` for product intent. Its "Current state"
   numbers were refreshed this session and are accurate as of `5ede17a`+;
   trust THIS file's table over anything else.
3. `HANDOFF.md`'s first `## CURRENT CURSOR:` block only. Everything below that
   block is preserved history, not a queue.
4. `projects/bullpen-rs/CLAUDE.md` and the workspace `AGENTS.md` before editing
   anything.

Do not read `CLAUDE-HANDOFF-2.md` or earlier unless you are chasing history.

## Verified state (measured 2026-09-20 00:02, not recalled)

| Item | State |
|---|---|
| HEAD | `b49bdfe` on `main`, tree clean |
| Gate worktree | `projects/bullpen-rs-gate`, clean, detached at `b49bdfe` |
| Last gate | **1,293 tests, EXIT 0** (`gate-b49bdfe.log`) |
| Live on meridian | **`b49bdfe`** — binary `ee7ab909bf9b8354...`, service active |
| Live client | tree `ec92063f...`; served wasm carries this session's strings |
| Live schema | `PRAGMA user_version = 23` |
| Live data | **21 bots, 37 skills, 16 `bot_skills` rows** |
| TS Bullpen `:4360` | untouched all session |
| Builders | none running; nothing uncommitted |
| GitHub | **`surreal1st/bullpen-rs`, PRIVATE**, `origin/main` = `b49bdfe` |

🔴 **The repo went to GitHub on 2026-09-20 and it is PRIVATE, deliberately.**
History was audited first: no secrets in the tree or in any of the 258 commits,
no blob over 200KB, and `.gitignore` already covers `*.db`, `.env`, keys,
`/dist`, `/shots`, `/target`. But the source comments and docs DO name Josh's
employer and clients (`rocketcom`, `Brassrook`), the tailnet hostname and
meridian's paths. **Do not make it public without Josh saying so** — that is a
one-way action, and the audit that cleared it for a private remote does not
clear it for a public one.

Live URL: `https://meridian.tail74afb5.ts.net:8452`. The plain-http IP answers
the API but the per-bot VM screen dies on it.

🔴 **Twelve untracked files sit in the repo root** — nine `gate-*.log` and three
`ship-*.log`. They are this session's evidence, they are not gitignored, and
nothing was deleted without asking Josh. A previous cursor said "eleven"; it
was wrong. `git status --porcelain` showing exactly these twelve is your
tree-clean check.

🔴 **The binary hash `ee7ab909...` is unchanged between `01bf4ab` and
`b49bdfe`, and that is CORRECT** — the last slice is client-only. **For a
client-only deploy the binary hash cannot verify anything.** Verify the CLIENT
tree (ship.sh prints its checksum) or, better, grep a new string out of the
SERVED wasm/css on the box. This session used
`sudo grep -rl 'no skill import in this client yet' /home/bullpen/bullpen-rs/client/`.

## What this session did

Four commits, four gate runs, three deploys. Each gated from the clean
worktree, hash-verified, and backed up by hand first (`install.sh` still does
NOT back up; backups live at
`/home/bullpen/bullpen-rs/backups/before-<sha>-<ts>/`).

- `4e1ee92` **say why a run produced nothing** (PORT-01)
- `6f66d01` **correct PROJECT.md** — §9a claimed a built feature was unbuilt
- `01bf4ab` **skills: the server half** (S10-01), migration 23
- `b49bdfe` **skills: in the app** (S10-02), client-only

Josh chose S10 as the slice and authorised the S8d spend.

---

## 1. S8d native GUI acceptance — EXECUTED, and its verdict

Josh authorised the spend this session. Full evidence, cost and reproduction:
`reviews/S8d-native-gui-acceptance-RESULT.md`. Runbook that was executed:
`tickets/S8D-ACCEPT-01-native-gui-proof.md`.

**Cost: `$0.050759`** of the ~`$0.82` headroom. Headroom after: **`$0.7699`**
(ceiling 10.0, usage 9.230102). Failed runs carry `usage=false` and are **not
billed**, which is why ~14 attempts cost 6 runs' worth.

### What passed

- ✅ **A bot can SEE its own screen.** A random token `MARBLE-9174-ERTJ` was
  put on the fixture's desktop via `xmessage` seconds before the ask; the bot
  returned it exactly. Every other route was denied (`browse`, `read_page`,
  `click`, `type_text`, `desk_shell`, `shell`, `review_media`, `escalate`).
  Proven by reading the `messages` table, not a tool result.
- ✅ **`desk_act`'s mouse physically acts.** Two independent observables:
  the pointer moved from `1023,767` to `403,530`, and window stacking changed.
- ✅ **`desk_act` correctly REFUSES a blind click.** A mouse action with no
  `observation_id` from a `snap_desk` is rejected. The half-blind guard works.

### What failed, and it is the important half

❌ **Aiming.** Neither model could hit an `okay` button whose centre is
~(412, 407) in a 1024x768 capture:

| Model | Clicked | Miss |
|---|---|---|
| `gemini-2.5-flash-lite` | (169, 518) | ~250px |
| `gemini-3.8-flash` | (403, 530) | x within 9px, **y off by 123px** |

Screen and capture are both 1024x768 (`xdotool getdisplaygeometry`), so this is
**not** a coordinate-space bug. It is model precision.

🔴 **Worse than missing: both models ASSERTED A SUCCESS THEY HAD NOT
ACHIEVED.** `gemini-2.5-flash-lite` reported "The xmessage dialog is no longer
on screen" while `xdotool` showed the window unchanged. Given an explicit
snap → act → snap → verify → correct loop, `gemini-3.8-flash` reported the
dialog "dismissed" when it had only been restacked. **The self-check does not
self-correct. A bot's own account of what it did is never evidence.**

Artifacts, both opened: `shots/s8d-accept-01-before-click.png`,
`shots/s8d-accept-02-after-clicks.png`.

🔴 **The fixture bot `s8d-acceptance-20260918` is a PERMANENT ROW on Josh's
live roster**, with its own VM container `bullpen-vm-s8d-acceptance-20260918`
(ports 9305/6205 — NOT `bullpen-desk`, which is live TS Bullpen's browser on
9223/6101 and was untouched). It is currently pinned to
`gemini-2.5-flash-lite` and holds several conversations, one of which
deliberately carries enough screenshots to reproduce the bug in §2. **Nothing
was deleted. Do not delete it without asking Josh.**

---

## 2. PORT-01 — the silent-run cause, proven and surfaced

### The defect

Runs died with `The model provider completed without an answer.` OpenRouter
sends the reason **on the choice**:

```json
{"choices":[{"delta":{"content":"","role":"assistant"},
  "finish_reason":"error","native_finish_reason":"MALFORMED_FUNCTION_CALL"}]}
```

`port.rs`'s `FrameChoice` modelled **neither** `native_finish_reason` nor any
choice-level error field, so serde discarded the diagnosis before any code
could see it. The top-level `frame.error` branch was always handled correctly;
a provider error arriving on the CHOICE never reached it. **That is why this
class of failure stayed undiagnosable for so long: the evidence arrived and was
destroyed inside the deserializer.**

### The reproduction

Deterministic, 3/3 each way, against the real API: with
`google/gemini-2.5-flash-lite`, **one** image in history succeeds; **two or
more** returns `MALFORMED_FUNCTION_CALL` every time.
`google/gemini-3.8-flash` handles three cleanly.

🔴 **BUT IT IS NOT IMAGE-ONLY.** It also hit `gemini-2.5-flash-lite` on a plain
tool-calling run with a large prompt and **no images at all** (bot `trinity`,
local copy, 2026-09-19). The image case is characterised; **this one is not.**
See §7 L1 — this is the most valuable loose thread in the project.

🔴 **The historical ~9% silent-run rate was measured on an image-free chat mix
and is STILL NOT established as the same bug.** Do not claim it is fixed.

### The fix, and the trap inside it

The fix surfaces `native_finish_reason` in both log lines and turns a
produced-nothing + `finish_reason == "error"` stream into a
`ModelEvent::Error` naming the reason.

🔴 **The trap the first version fell into, and it is worth internalising.**
`provider_error(raw, key, image_request)` **discards its message entirely**
when `image_request` is true:

```rust
if image_request {
    return "The model provider rejected the screen observation without exposing its payload.".to_string();
}
```

…and `image_request` is just `carries_image(&request)` (`port.rs:369`, passed
at `:469`). So routing the new server-generated message through it made the fix
**INERT on every screen-observation run — the one case it exists for.** It was
caught by READING THE HELPER, not by reading the diff or running the tests.

`provider_error` itself is correct and untouched: a provider-composed error
body can echo an image payload back. Only the provider-derived token is
sanitised — 1–64 chars of `[A-Za-z0-9_.-]`, as an anchored full match via
`chars().all`, never `Regex::is_match` (which is a SEARCH; this project already
has a scar from exactly that, see PROJECT.md §5).

### Proven from the running system

Not from a hash. The same request on live now answers
`The model provider stopped with an error: MALFORMED_FUNCTION_CALL.` and the
journal carries `native_finish_reason="MALFORMED_FUNCTION_CALL"`.

---

## 3. S10-01 — skills, the server half (`01bf4ab`, migration 23)

Library, per-bot enable, the `use_skill` tool, and the one-line-per-skill
prompt index. Ported from `bullpen-night/src/server/skills.ts` (247 lines).

**Routes:** `GET/PUT/DELETE /api/skills/{name}`, `GET /api/skills` (body
STRIPPED, each carrying `bytes`), `GET /api/bots/{id}/skills`,
`PUT /api/bots/{id}/skills/{name}`.

🔴 **An enabled skill contributes ONE LINE to the prompt** — its name and when
to use it. The body arrives only when the bot calls `use_skill`. A test proves
the prompt carries the description and **never** the body, because a regression
there is invisible except as a bill.

🔴 **The live database ALREADY had `skills` and `bot_skills`, with 37 skills
and 16 enablements written by the TS product.** That is why both tables are
`CREATE TABLE IF NOT EXISTS` — a correctness requirement, not defensive
padding. Validated against a real copy before shipping; the migration ran on
live and preserved every row.

🔴 **`DELETE /api/skills/{name}` is built and tested but deliberately wired to
NO control in the client.** Putting a delete button in front of Josh is his
decision.

**Artifact:** `shots/s10-01-skill-followed.png`, opened. Trinity answers in
exactly three bullet points then the word `PELICAN` — both instructions exist
ONLY in the skill's body, which the prompt never carries, so the model can only
have seen them by calling `use_skill`.

🔴 **Appending ANY migration reds four pre-existing tests** that hardcode the
latest `user_version`: `store/tests/migrations.rs`, `store/tests/memory.rs`,
`store/tests/routines.rs`, `server/tests/judge.rs`. Expect that. Note
`fixture_db_opens_unchanged` asserts **exact set equality** on
`sqlite_master` — add the new schema object names to its expected-new set
rather than loosening the assertion. Migration 23 adds five: `skills`,
`bot_skills`, `sqlite_autoindex_skills_1`, `sqlite_autoindex_skills_2`,
`sqlite_autoindex_bot_skills_1`.

## 4. S10-02 — skills, in the app (`b49bdfe`, client-only)

A library section in Settings (descriptions only; a body fetched on click) and
a checkbox per skill in a bot's Edit modal.

🔴 **A toggle's new state comes from the SERVER'S answer, never the click**,
and a failed request leaves the checkbox exactly as it was. That rule lives in
`skill_set_after_response`, which returns `None` on error and therefore has
nothing to paint an optimistic guess FROM. `Ok(empty)` stays `Some(empty)` —
"the server says zero skills" and "the request failed" are different facts and
must not collapse.

🔴 **The TS's "shared computer" block is deliberately NOT ported.** It calls
`GET /api/desk` and links to `/desk/`, which in this product is **live
TypeScript Bullpen's own browser** on :9223/:6101. This product uses per-bot
VMs (`vm::vm_desk`). PROJECT.md §7 forbids new code reaching `desk_config(env)`.

The empty state does not name `npm run import-skills` either — that command
does not exist in this product.

**Artifacts, both opened:** `shots/s10-02-skills-library.png` (anti-slop
expanded, body fetched on click) and `shots/s10-02-bot-skills.png` (Arthur's
Edit modal, `anti-slop` ticked — a real enablement the TS product wrote).

---

## 5. 🔴 Open, and they need JOSH — do not decide these

1. **The cheap floor collides with computer use.** `modelForRun` enforces a
   hard cheap floor for unattended runs, and the cheapest vision model provably
   cannot survive a second screenshot. An unattended computer-use loop fails —
   loudly now, thanks to PORT-01, but it still fails.
2. **Should run history carry more than the latest observation?** Every extra
   screenshot is re-paid on every later step of the same run.
3. **A delete control for skills.** Route exists and is tested; the button is
   his call.
4. **Should a failed run leave a trace in the thread after a reload?**
5. **Two identical `+` buttons in the rail** (group chat, new bot), and
   **export is two clicks** (fetch, then save).
6. **45 stale `node` processes, ~2.5 GB** — `qmd` MCP servers and Codex
   `cua_node` runtimes from sessions back to 09-16. Awaiting his yes to kill.
7. **Is the CPU cooler still degraded?** If so, heavy builds belong on meridian.
8. **Two stray files this session created and did not delete**, awaiting his
   yes: `.scratch/bullpen-rs/HANDOFF.md.bak-before-cursor` and
   `.scratch/bullpen-rs/NEW-CURSOR.tmp`.

**ANSWERED this session, do not re-ask:**
- An unlisted `model:` on import **keeps REFUSING the whole file** (asked three
  times across two sessions; now settled).
- S8d spend: **approved and spent.**
- Next slice after S8d: **S10.**

---

## 6. 🔴 Latent holes recorded, NOT fixed

- **`store::rename_thread` has NO empty-title check.** Its local is named
  `trimmed` while doing `chars().take(120)` — a length CAP, not a trim. An
  empty title does not 404; it silently **wipes** a real conversation's title.
  The client guard added in `5ede17a` is the only thing preventing it, and any
  other caller (a script, the iOS client) can still do it. Matches the TS.
- **Nothing in this port writes `voice`.** No route, no store setter.
  `duplicate_bot` is the only code that ever sets that column.
- **`store::get_bot` returns ARCHIVED bots** (no `archived_at` filter).
  `routes/import.rs`'s duplicate check depends on this — do NOT add the filter.
- **`PUT /api/bots/{id}/permissions` REPLACES the whole set, it does not
  merge.** A PUT carrying only `{"escalate":"deny"}` silently reset `browse`
  back to allow. Caught only by re-reading with a fresh GET. If you set
  permissions, send the COMPLETE set and verify with a separate read.

---

## 7. What is left

### L1 — characterise the no-image `MALFORMED_FUNCTION_CALL` (highest value, no decision needed)

The image case is deterministic and understood. The no-image case was hit once
and not chased. Until it is characterised, "when does this bite" is unknown on
ordinary chat runs — which is most of the product. Nothing blocks this.

### N1 — finish S10 (no decisions in it)

Client skill creation/editing, skill import, marketplace (migration 10 already
carries its tables — leave them alone until then), `hire_bot`, `propose_tool`,
template export scrub, hard-delete-bot.

### N2 — the remaining slices, each a slice-sized decision or bigger

S7 connectors (MCP gateway + OAuth 2.1, needs a design pass) · S9 jobs, repo
tools + PR, `spawn_helper`, second Docker host · S11 people and channels ·
S12 media and money · S13 mobile (iOS via Dioxus) · S14 cutover.

### Also available

`tool_choice` with a per-model capability flag (PROJECT.md §9b) — decided by
Josh 2026-09-17, insurance rather than a blocker. `gemini-3.8-flash` obeys
`required`; `qwen3.8-flash` returns HTTP 400; `gpt-oss-120b` ignores it. So it
cannot be a global setting.

---

## 8. 🔴 The recipe: screenshotting anything that CREATES a row

A successful create makes a real row, and **nothing may be deleted without
asking Josh**, so it cannot be driven on live. A blank local db cannot stand in
either: **there is no password bootstrap in `main.rs`**, so a fresh
`BULLPEN_DATA_DIR` has no credential and every `/api/*` route answers 503.

1. `ssh meridian "sudo cp <a backups/before-*/bullpen.db> /tmp/bullpen-copy.db
   && sudo chmod 644 /tmp/bullpen-copy.db && sha256sum ..."`, `scp` it to a
   scratch dir as `bullpen.db`, **and check both hashes** — a file move between
   these two machines has silently transcoded before.
2. `CARGO_BUILD_JOBS=10 cargo build -p server` (DEBUG — `BULLPEN_FAKE_PORT` is
   `#[cfg(debug_assertions)]` only) and `bash scripts/build-client.sh`.
3. Run `target/debug/bullpen.exe` with `BULLPEN_DATA_DIR` = that scratch dir,
   `BULLPEN_CLIENT_ROOT` = absolute `dist/client`, `BULLPEN_PORT=4381`,
   `BULLPEN_HOST=127.0.0.1`. Windows paths need `cygpath -w`.
4. `node scripts/shot-signed-in.mjs http://127.0.0.1:4381/ <out.png> "<js>"`.
   The real password works because the copy carries the real password row.
5. Stop it by PID afterwards and PROVE it died (port clear AND pid gone).

**Four traps inside that recipe, all hit:**

- `shot-signed-in.mjs` already declares `const sleep` AND `const input`, so
  injected JS redeclaring either dies with a `SyntaxError` before touching the
  page.
- 🔴 **`BULLPEN_FAKE_PORT=1` returns a CANNED reply with no tool calls.** A
  screenshot driven with it proves nothing about a real model. For anything
  involving a real run, omit it and set
  `BULLPEN_OPENROUTER_KEY_FILE=C:\Users\rain\.bullpen\openrouter.key` (the PATH
  on the command line is fine; the KEY never is). A real run costs real money
  against the same ceiling.
- A fake-port instance has an EMPTY model catalogue, so `judge_pin` refuses
  every pinned model with "OpenRouter does not list this model" — environment,
  not a bug.
- Rail rows are `<button>` elements. Click them by
  `[...document.querySelectorAll('button')].filter(b => b.textContent.includes('Trinity'))[0].click()`
  — an ancestor-traversal selector from a leaf text node silently does nothing.

A hash-verified copy of the live db from `before-5ede17a-20260919-004336`
(sha256 `4e3f4576d28c0184...`) was used this session. The session scratchpad is
gone; take a fresh copy.

---

## 9. Method, and what it actually caught

One source-writing builder at a time (Sonnet), against a written ticket in
`.scratch/bullpen-rs/tickets/`. The coordinator re-runs every test, writes and
runs the mutations itself, takes the screenshot, commits with explicit paths,
gates from the clean worktree, ships, and verifies. **A builder's pasted green
is never evidence. Neither is a code trace — prove runtime behaviour by reading
the database.** Gate at checkpoints, not per slice. `CARGO_BUILD_JOBS=10`.

### 🔴 The lesson of this session: a mutation can go RED and still prove nothing

A builder invented a helper that discarded three of its four parameters:

```rust
fn skill_set_after_toggle(before, name, requested_on, server_names) -> HashSet<String> {
    let _ = (before, name, requested_on);
    server_names.into_iter().collect()
}
```

Its test proved that a function ignoring its inputs returns its remaining
input. The mutation bit, the test went red, the process looked correct — and
the one behaviour that mattered (a failed toggle painting a checkbox on) had
**zero coverage**. It was replaced with a seam carrying the real decision,
including the failure path.

This is the sibling of "a mutation that stays green is a defective test":
**a mutation that goes red against a defect nobody would ever write.** Only
catchable by reading what the test GUARDS, never by watching it flip colour.

### 🔴 A coordinator's own verification tool can lie

It happened twice. A mutation harness reported a test as "failed to build" when
it had panicked correctly. Later it reported three REDs that were really just
`cargo test -p client --lib` failing because **the `client` crate has no lib
target** (use `cargo test -p client`). **Always read the evidence line, never
the summary.**

### 🔴 Builders correctly corrected the coordinator's tickets THREE times

1. The raw-JSON frame test pattern lives in `crates/model/tests/port.rs`, not
   `port.rs`'s in-module block.
2. `use_skill -> Allow` was already in `permissions.rs` before the ticket asked
   for it to be added.
3. `settings.rs` sections use `stg-sub`/`stg-sub-h`; `set-block`/`set-h` are
   the TS source's class names, quoted into a ticket without checking.

Write tickets from the CODE, not from the last handoff's summary of it — and
expect to be corrected.

### Other things only a non-code check caught

- **A panic reachable from an untrusted request body**, found by reading a
  builder's diff (`strip_md_extension` slicing a `str` at a non-boundary).
- **A missing test, found by a SURVIVING mutant**: reordering `detect_format`'s
  basename checks passed the entire suite.
- **Defects only a screenshot could find**: an import preview showing no
  instructions; a conversation chip reading "Untitled 0" while already titled.

🔴 **Restores must be verified.** New files are untracked, so `git diff` shows
nothing and cannot prove a mutation was undone — hash the files BEFORE mutating
and verify byte-identity after. Restore by re-editing or from a saved buffer,
**never `git checkout --`**, which would wipe every other uncommitted edit.
Work in binary mode: Python/Node text mode on Windows rewrites line endings.

🔴 **Never pipe the gate through `grep`/`tail`/`head`.** The pipeline's exit
status replaces the gate's, so a red suite reports success. Redirect to a file
and grep the file.

🔴 **Ship from the CLEAN GATE WORKTREE**; `ship.sh` builds from whatever tree it
runs in. Update it with
`git -C projects/bullpen-rs-gate checkout --detach <sha>` — never COMMIT from
there.

---

## 10. Suggested next move

**L1** — characterise the no-image `MALFORMED_FUNCTION_CALL`. It needs no
decision from Josh, it is cheap, and it is currently the difference between
"we know when this bites" and "it bites sometimes on ordinary chat."

If Josh wants visible progress instead, **N1** (finish S10) is entirely
decision-free.

Everything in §5 is his and must not be decided for him.
