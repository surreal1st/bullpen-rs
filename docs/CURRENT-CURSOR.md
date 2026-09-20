# Current cursor

The live cursor block, extracted from `.scratch/bullpen-rs/HANDOFF.md` on the
workstation. **That file is the source of truth** and also carries every
superseded cursor as history; this is only its first block, which is the one
an agent picking the project up should read.

Read `docs/CLAUDE-HANDOFF-4.md` first - it is the full handoff.

---

## CURRENT CURSOR: 2026-09-19 02:50 - b49bdfe LIVE; S10-01 + S10-02 shipped

🔴 **Read `CLAUDE-HANDOFF-4.md` FIRST.** It is the full handoff out of this
session and supersedes `CLAUDE-HANDOFF-3.md` (the handoff INTO it). It carries
S8d, PORT-01, S10-01 and S10-02 in full, the open decisions, the recipes and
the method lessons. Everything below this block is preserved history, not a
queue.

HEAD `b49bdfe` on `main`, tree clean (eleven untracked `gate-*.log`/`ship-*.log`
files are this session's evidence; nothing deleted without asking Josh).
**Gate: 1,293 tests, EXIT 0** (`gate-b49bdfe.log`, clean gate worktree).
**Live on meridian: `b49bdfe`.**

🔴 **Binary hash is UNCHANGED at `ee7ab909bf9b8354...` and that is CORRECT** -
`b49bdfe` is client-only. The deploy was verified by finding TWO new strings
("no skill import in this client yet", "one line to this bot") in the SERVED
wasm on the box, not by a tree hash. Client tree `ec92063f...`.

Schema 23, 21 bots, **37 skills, 16 `bot_skills` rows** - all preserved across
both deploys.

Four commits this session: `4e1ee92` PORT-01, `6f66d01` PROJECT.md correction,
`01bf4ab` S10-01 (skills server half), `b49bdfe` S10-02 (skills in the app).

### S10-02: skills in the app - LIVE

A library section in Settings (descriptions only; a body is fetched on click)
and a checkbox per skill in a bot's Edit modal.

🔴 **A toggle's state comes from the SERVER'S answer, never the click**, and a
failed request leaves the checkbox alone. That rule lives in
`skill_set_after_response`, which returns `None` on error and so has nothing to
paint an optimistic guess FROM. `Ok(empty)` stays `Some(empty)` - "the server
says zero skills" and "the request failed" must not collapse.

🔴 **The TS's "shared computer" block is deliberately NOT ported.** It calls
`GET /api/desk` and links to `/desk/`, which is live TypeScript Bullpen's own
browser on :9223/:6101. This product uses per-bot VMs.

🔴 **No delete control anywhere.** `DELETE /api/skills/{name}` exists and is
tested; a delete button is Josh's decision.

Artifacts, both opened: `shots/s10-02-skills-library.png` (anti-slop expanded,
body fetched on click) and `shots/s10-02-bot-skills.png` (Arthur's Edit modal,
`anti-slop` ticked - a real enablement the TS product wrote).

### 🔴 The method lesson of this session, worth more than the code

**A mutation can go RED and still prove nothing.** The first S10-02 test
invented a helper that discarded three of its four parameters, then proved
that a function ignoring its inputs returns its remaining input. The mutation
bit, the test went red, the process looked correct - and the one behaviour
that mattered (a failed toggle painting a checkbox on) had zero coverage.
Sibling of "a mutation that stays green is a defective test": **a mutation
that goes red against a defect nobody would ever write.** Only catchable by
reading what the test guards.

Twice more the harness itself lied: it reported a test as "failed to build"
when it had panicked correctly, and later reported three RED verdicts that
were really just `cargo test -p client --lib` failing because **the `client`
crate has no lib target**. Always check the evidence line, never the summary.

### 🔴 Builders corrected MY tickets three times. Each was right.

1. The raw-JSON frame test pattern lives in `crates/model/tests/port.rs`, not
   `port.rs`'s in-module block.
2. `use_skill -> Allow` was already in `permissions.rs` before the ticket asked
   for it.
3. `settings.rs` sections use `stg-sub`/`stg-sub-h`; `set-block`/`set-h` are
   the TS source's class names, which I quoted without checking.

### 🔴 Open, and they need JOSH

1. **The cheap floor collides with computer use.** `modelForRun` holds a hard
   cheap floor for unattended runs; the cheapest vision model cannot survive a
   second screenshot. It fails LOUDLY now, but still fails.
2. 🔴 **`MALFORMED_FUNCTION_CALL` is NOT image-only** - it also hit
   `gemini-2.5-flash-lite` on a plain tool-calling run with a big prompt and
   NO images (Trinity, local copy). The image case is deterministic (2+
   screenshots, 3/3); this one is uncharacterised.
3. Should run history carry more than the latest observation?
4. A delete control for skills - deliberately not built.
5. Should a failed run leave a trace in the thread after a reload?
6. Two identical `+` buttons in the rail; export is two clicks.
7. 45 stale `node` processes, ~2.5 GB, awaiting his yes to kill.
8. Is the CPU cooler still degraded?

Also awaiting his yes: two stray files I made and did not delete -
`.scratch/bullpen-rs/HANDOFF.md.bak-before-cursor` and `NEW-CURSOR.tmp`.

### What is left of S10

Client skill creation/editing, import, marketplace (migration 10 already
carries its tables), `hire_bot`, `propose_tool`, template export scrub,
hard-delete-bot.
