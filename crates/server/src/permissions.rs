//! S2-02: what a bot is allowed to do, per bot. Port of
//! `projects/bullpen-night/src/server/permissions.ts`.
//!
//! The single most important property, and the one Rakazo got wrong: **these
//! rules are the only authority.** Rakazo hardcoded a "safe tool set" that
//! webhook-triggered runs were locked to, and the code never consulted the
//! approval rules at all in that branch, so no configuration could ever unblock
//! it. Winston could think and answer correctly and had no path to a voice.
//!
//! Here the answer to "may this tool run" depends on the bot and the tool. It
//! does not depend on what started the run.

use model::ladder::Trigger;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// A decision on what a tool may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// The tool may run without asking.
    Allow,
    /// The tool needs approval.
    Ask,
    /// The tool may not run.
    Deny,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Decision::Allow),
            "ask" => Some(Decision::Ask),
            "deny" => Some(Decision::Deny),
            _ => None,
        }
    }
}

pub type Permissions = HashMap<String, Decision>;

/// Defaults by what a tool can do, not by who called it.
///
/// Reading is free. Writing to the bot's own memory is free, because the blast
/// radius is the bot. Running a shell command asks, because that is the one that
/// can delete something.
pub fn default_decisions() -> Permissions {
    let mut m = HashMap::new();

    // Asking for a better model is a bot admitting it is stuck, not an action
    // that needs signing off. Left at ask it would pause the run for approval,
    // which is exactly the friction escalation exists to remove.
    m.insert("escalate".to_string(), Decision::Allow);

    // Asking a colleague is normal. It costs a little money, which the spend
    // ceiling already governs, and the depth limit stops it running away.
    m.insert("message_bot".to_string(), Decision::Allow);

    // A nameless helper for a side errand: the cheap model always, a tool list
    // narrowed to its one job, and the CALLER's own permissions rather than
    // anything wider. The cost folds into the run that spawned it, the same as
    // message_bot, so it is governed by the same spend ceiling and depth limit.
    m.insert("spawn_helper".to_string(), Decision::Allow);
    m.insert("search_memory".to_string(), Decision::Allow);

    // W1: reading Josh's memory vault. Reads are allow, the write is not.
    //
    // The vault is his own notes, and the bots that get it are the ones he
    // switched it on for - the opt-in IS the permission, exactly as it is for
    // skills, so a second prompt in front of every search would be asking him to
    // re-grant per call what he already granted per bot.
    //
    // 🔴 `vault_write` ASKS, and it is in TIGHTEN below. This is the one store in
    // the product that is not recoverable from anywhere else: it is where his
    // working rules, his locked decisions and his project history live, and every
    // other memory in Bullpen is scoped to one bot while this is shared by all of
    // them. Versions mean a bad write is recoverable, which is a reason to keep
    // the history, not a reason to skip the approval.
    m.insert("vault_search".to_string(), Decision::Allow);
    m.insert("vault_read".to_string(), Decision::Allow);
    m.insert("vault_write".to_string(), Decision::Ask);

    // W2: read-only search over IMPORTED past sessions (Claude Code, Cursor,
    // plain exports). Same reasoning as vault_search - the opt-in toggle IS the
    // permission, so asking again per call would just be re-asking what Josh
    // already granted when he switched it on for this bot.
    m.insert("search_history".to_string(), Decision::Allow);

    // Searches a bot's own past conversations; reads its own threads only.
    m.insert("search_conversations".to_string(), Decision::Allow);
    m.insert("fetch_url".to_string(), Decision::Allow);

    // Reading resources from a connector Josh already enabled. Same trust as fetch_url.
    m.insert("list_resources".to_string(), Decision::Allow);
    m.insert("read_resource".to_string(), Decision::Allow);

    // A public read of who is streaming, on a fixed host, with no credential and
    // nothing written anywhere. Same trust level as fetch_url, and pausing a
    // 30-minute live ping for approval would defeat the routine entirely.
    m.insert("twitch_live".to_string(), Decision::Allow);
    m.insert("twitch_live_ping".to_string(), Decision::Allow);
    m.insert("remember".to_string(), Decision::Allow);

    // Reading a skill Josh already gave this bot. It is text he wrote or imported,
    // it grants no tool and widens no permission, and asking would stall a run
    // behind a prompt for instructions the bot was handed on purpose.
    m.insert("use_skill".to_string(), Decision::Allow);

    // Saying something mid-run, and asking Josh a question.
    //
    // 🔴 Both allow, and `ask_josh` in particular. Putting an approval in front of
    // "may I ask you a question" is a prompt to answer a prompt, and the whole
    // reason the tool exists is that a routine which cannot ask has to GUESS.
    // Neither writes anything outside this bot's own conversation.
    m.insert("say".to_string(), Decision::Allow);
    m.insert("ask_josh".to_string(), Decision::Allow);

    // Reading and writing the bot's own checklist. The blast radius is its own
    // rows, the same reason `remember` is allow.
    m.insert("add_task".to_string(), Decision::Allow);
    m.insert("update_task".to_string(), Decision::Allow);
    m.insert("list_tasks".to_string(), Decision::Allow);

    // S2: a goal's own tools. The blast radius is the calling bot's own goal
    // rows - `updateGoal`'s `botId` scoping refuses another bot's goal id the
    // same way `updateTask` does - so these get the same trust `add_task` and
    // `update_task` do.
    m.insert("set_goal".to_string(), Decision::Allow);
    m.insert("update_goal".to_string(), Decision::Allow);
    m.insert("reflect".to_string(), Decision::Allow);

    // A keyless read of a public search page on a fixed host. Same trust level as
    // `fetch_url`, which it feeds: what a search RETURNS still goes through the
    // resolver check before anything fetches it.
    m.insert("web_search".to_string(), Decision::Allow);

    // Reading and stopping the bot's own background work. Both are about jobs it
    // started itself, so the blast radius is its own rows.
    m.insert("job_status".to_string(), Decision::Allow);
    m.insert("stop_job".to_string(), Decision::Allow);

    // Waiting on the bot's own job.
    m.insert("await_job".to_string(), Decision::Allow);

    // S10: closing an interactive shell session. Always allow, the same
    // reasoning `stop_job` gets - it only ever tears down something the bot
    // already holds, never starts anything new. `shell_open`/`shell_write`/
    // `shell_read` are NOT here: they alias onto `shell` itself, in
    // `get_permissions` below, rather than holding a row of their own.
    m.insert("shell_close".to_string(), Decision::Allow);

    // 🔴 Starting background work ASKS, and it is in TIGHTEN below.
    //
    // `run_in_background` is `shell` with the timeout taken off, so it inherits
    // shell's decision rather than being a cheaper way to reach the same place. A
    // bot that could not run `rm -rf /work` for 30 seconds must not be able to run
    // it for two hours unattended.
    m.insert("run_in_background".to_string(), Decision::Ask);

    // 🔴 Asking a colleague in the background ASKS, where `message_bot` allows,
    // and the difference is money rather than danger. A synchronous ask is bounded
    // by the asking bot's own turn; four background ones are four model runs
    // nobody is watching, against a $60 ceiling.
    m.insert("ask_in_background".to_string(), Decision::Ask);

    // Drawing costs money per picture and produces something Josh looks at, so it
    // is his call the first time.
    m.insert("draw_image".to_string(), Decision::Ask);

    // W4a: `deliver` (pdf/spreadsheet/deck/clip/text). Allow, and NOT in TIGHTEN
    // below - unlike `draw_image` it costs nothing and only ever writes into
    // Bullpen's own attachment store, which an unattended routine producing a
    // report for Josh to read later is exactly what this is for. `clip`'s own
    // path check (deliverables.ts's `resolveClipSource`) is what keeps it from
    // reading an arbitrary file regardless of this setting - a permission tier
    // says WHO may call the tool, not what a `path` argument may name.
    m.insert("deliver".to_string(), Decision::Allow);

    // Looking at the shared computer's own screen.
    //
    // 🔴 Allow, and the reasoning is the same as `read_page`: this is a READ of a
    // machine the bot is already driving, and it produces something JOSH looks at.
    // It is also the only tool that can contradict a bot's own account of what it
    // did on the desk, which is worth having reachable without a prompt.
    //
    // It does NOT widen what the desk can reach - `click` and `type_text` are
    // still held back, so a bot can watch itself and still not press anything.
    m.insert("snap_desk".to_string(), Decision::Allow);
    m.insert("record_desk".to_string(), Decision::Allow);

    // Building the Broadcast page. It renders text into this bot's own
    // conversation and reaches nothing else - the same blast radius as `say` -
    // and Jason's whole job is producing this, so an approval in front of it is
    // friction on the one task he exists for.
    m.insert("zenith_html".to_string(), Decision::Allow);

    // Reading another bot's finished run: returns the trace with tool calls and results.
    // The bot can only read runs of other bots in the same roster, so no new data access.
    m.insert("read_run".to_string(), Decision::Allow);

    // Moving a thread from another bot to this one. Refuses rooms and the bot's own threads.
    // Requires Josh to see the thread move, so it asks by default but tightens in unattended runs.
    m.insert("adopt_thread".to_string(), Decision::Ask);

    // 🔴 Reading the web through the shared browser. Same trust level as
    // fetch_url - a read of a public page - and refused the same way: the
    // server checks the address before navigating, and Chromium's own resolver
    // refuses every private range including on redirect.
    //
    // What is NOT allowed by this is acting AS Josh anywhere. The desk's browser
    // carries whatever sessions he has signed in, which is the point and also the
    // risk, so the two tools that can push a button there are held back below.
    m.insert("browse".to_string(), Decision::Allow);
    m.insert("read_page".to_string(), Decision::Allow);

    // 🔴 Clicking and typing ALLOW in a chat, and are in TIGHTEN below, which is
    // the whole of the design: free while Josh is sitting in front of the
    // conversation, pulled back to "ask" the moment a routine or a goal session
    // runs one unattended.
    //
    // They asked by default until 2026-09-14, and the note here argued that
    // pressing a button on a page where Josh is signed in is an ACTION taken as
    // him. That is still true and is still why TIGHTEN holds them. What the
    // argument missed is what these two tools are actually made of: SEARCHING a
    // site is typing into its search box. A bot handed `browse` and `read_page`
    // and told to look something up on SAM.gov opens the page, cannot type, and
    // hands the job back - which reads as a bot refusing to work. Josh: "I need
    // the bots outfitted with every permission they need to use the computer.
    // They're still pushing back on me."
    //
    // An approval in front of every keystroke is not a decision he is making
    // anyway; it is a prompt he clears to get the search he already asked for.
    // The approval that means something is the unattended one, and that is
    // exactly the one TIGHTEN keeps. `desk_shell` and `shell` still ask
    // everywhere - those are not what searching a site is made of.
    m.insert("click".to_string(), Decision::Allow);
    m.insert("type_text".to_string(), Decision::Allow);

    // The desk's terminal. Asks for the same reason `shell` does, and harder: the
    // sandbox is throwaway and offline, while this machine is persistent, has the
    // internet, and holds the browser profile with every shared cookie in it.
    m.insert("desk_shell".to_string(), Decision::Ask);

    // 🔴 Coordinate-level input on the shared desk - clicking, typing and
    // pressing keys by pixel position rather than by reading a page.
    //
    // Follows `click`/`type_text` exactly, and for the same reason it always
    // has: allow in a chat, TIGHTEN below for anything unattended. It is the
    // half of "use the computer" that reaches past the browser - a native
    // dialog, a file picker, an app with no DOM - so leaving it asking while
    // the other two are free would just move the wall one step further in.
    m.insert("desk_act".to_string(), Decision::Allow);

    // S4: watching a video or reviewing an attachment. Asks by default -
    // `desk_shell`'s reasoning applies just as hard here: it runs yt-dlp and
    // ffmpeg in the persistent desk container, and a `url` reaches the open
    // internet from inside it (behind `mayVisit`, the same fence `browse`
    // checks, but a fence is not a reason to skip the approval too).
    m.insert("watch_video".to_string(), Decision::Ask);
    m.insert("review_media".to_string(), Decision::Ask);

    // S1c: doing again, on the bot's own machine, something Josh demonstrated.
    //
    // 🔴 Asks, and it is in TIGHTEN below. A replay is `click` and `type_text`
    // in a sequence he wrote, on the browser that carries his logins - so it
    // cannot sit at a trust level LOOSER than the two tools it is made of, both
    // of which ask. That he demonstrated it is consent for the steps, not
    // standing consent to run them again whenever a page or a trigger asks.
    m.insert("replay_demo".to_string(), Decision::Ask);
    m.insert("shell".to_string(), Decision::Ask);

    // Reading a file on Josh's OWN computer. Asks for the same reason ssh does:
    // fetch_url reaches the open internet, so a bot that can read his disk
    // without being asked is the whole lethal trifecta in one bot. The desktop
    // app refuses credential-shaped paths underneath this regardless.
    m.insert("read_file".to_string(), Decision::Ask);

    // The widest thing a bot can do. It runs on meridian itself, outside the
    // sandbox, as a user that owns the projects - so it asks by default and a
    // routine can never quietly promote it (see tightenForRoutine).
    m.insert("ssh".to_string(), Decision::Ask);

    // S3: a bot's own checkout in `/work/repo`, offered only when it has a
    // `repo` set. Reading is free, same reasoning as `browse`/`read_page`:
    // `repo_read`/`repo_grep` cannot change anything. `repo_run` is free too -
    // it is `shell` inside a checkout rather than an empty sandbox, and `shell`
    // itself is "ask" only because it can touch the bot's OWN volume; a repo
    // checkout is exactly that same volume. `repo_branch` only ever moves a
    // LOCAL ref, never pushes, so it gets `add_task`'s trust rather than
    // `shell`'s. `repo_edit` and `repo_pr` ask: an edit changes files Josh may
    // read back, and a PR reaches GitHub and is the one action here with a
    // blast radius outside this bot's own sandbox.
    m.insert("repo_read".to_string(), Decision::Allow);
    m.insert("repo_grep".to_string(), Decision::Allow);
    m.insert("repo_run".to_string(), Decision::Allow);
    m.insert("repo_branch".to_string(), Decision::Allow);
    m.insert("repo_edit".to_string(), Decision::Ask);
    m.insert("repo_pr".to_string(), Decision::Ask);

    // W5: a bot writing a NEW TOOL for the whole roster.
    //
    // 🔴 Asks, it is in TIGHTEN below, and `decide_call` pins it to "ask" whatever
    // the stored map says - three separate reasons, because this is the one tool
    // whose result is more tools. A proposal that Josh waves through is code that
    // every bot can then call, so "always allow" on it would not be a convenience,
    // it would be handing the roster the ability to grow its own capabilities
    // while he is asleep.
    //
    // What the approval is NOT is a code review. It cannot be: Josh reviews
    // rendered artifacts, not source. What the card gives him is the name, the
    // bot, what it claims to do, and the worked examples it actually ran and
    // passed - and the source underneath for anyone who wants it. Everything that
    // can be judged mechanically was judged before he was asked.
    m.insert("propose_tool".to_string(), Decision::Ask);

    // W9: `query_db` against a named read-only database target.
    //
    // Asks by default, and it is in TIGHTEN below. The parser and the sqlite
    // `readOnly` flag already refuse a write whatever this setting says - what
    // this decision governs is the READ itself, and CRM, GlassDex and Repull
    // all hold data Josh has not chosen to hand a bot standing access to.
    m.insert("query_db".to_string(), Decision::Ask);

    // S7: buying something on the bot's own virtual card.
    //
    // 🔴 Asks, and it is in TIGHTEN below, and `decide_call` pins it to "ask"
    // whatever the stored map says - the same three-part lock `propose_tool`
    // gets, for the same reason: real money leaving the building is not a
    // decision a grid checkbox or a natural-language rule gets to make alone.
    // "Never" (deny) still works, for a bot with no business buying anything.
    m.insert("purchase".to_string(), Decision::Ask);

    // W?: `hire_bot` - putting a new, permanent bot on the roster.
    //
    // 🔴 Asks, and it is in TIGHTEN below. Unlike a colleague asked a question
    // (`message_bot`, allow) this is a standing addition to the roster that
    // outlives the run that made it, the same shape `propose_tool` has for
    // tools rather than bots - so a 06:00 routine must not be able to hire
    // someone with nobody awake to see who just joined.
    m.insert("hire_bot".to_string(), Decision::Ask);

    m
}

/// Tools a routine may never hold on "allow".
///
/// Each one reaches something outside the bot's own sandbox: his machine, the
/// host, or a shell. An unattended run at 06:00 has nobody to object.
pub fn tighten_set() -> Vec<&'static str> {
    vec![
        "shell",
        "ssh",
        "read_file",
        // 🔴 The desk's terminal and its two acting tools, for exactly the reason the
        // set exists. A 06:00 routine driving a browser Josh is signed into, on a
        // machine that keeps its files, with nobody awake to answer for it, is the
        // widest thing this platform can do unattended. Reading stays allowed:
        // `browse` and `read_page` are not in here, so a routine can still go and
        // look at something, which is what routines are for.
        "desk_shell",
        "click",
        "type_text",
        // Same reasoning again: it presses buttons and reaches further than click
        // and type_text, since it is not limited to what CDP can see in the browser.
        "desk_act",
        // 🔴 S1c: a replay is a SEQUENCE of `click` and `type_text`, so it has to be
        // here wherever they are - a routine that could hold `replay_demo` on
        // "allow" would be a 06:00 run pressing twenty buttons in Josh's signed-in
        // browser, which is the exact thing the three names above it are kept out of
        // unattended runs to prevent. It is also the shape that looks most harmless:
        // "it only does what he showed it" is true of the recording and says nothing
        // about the page it lands on tonight.
        "replay_demo",
        // 🔴 Background work, for the reason the set exists. `run_in_background` is
        // `shell` with the 30-second cap removed, so an unattended run could otherwise
        // hold a container on meridian for two hours with nobody awake to object -
        // on the box that runs SHOOT, both CRMs, Brassrook and Switchboard.
        //
        // `ask_in_background` is here on COST rather than reach: a routine firing
        // every 30 minutes that starts four colleague runs each time is sixteen
        // unwatched model runs an hour against a $60 ceiling.
        "run_in_background",
        "ask_in_background",
        "draw_image",
        // Adopting a thread is a structural change to ownership. An unattended run
        // must not shuffle threads around without Josh seeing it.
        "adopt_thread",
        // W1: writing to Josh's memory vault, for the reason the set exists. A 06:00
        // routine rewriting the note that holds a LOCKED decision, with nobody awake
        // to object, is the widest thing the memory half of this platform can do -
        // and unlike a shell command it leaves the system looking healthy afterwards.
        "vault_write",
        // S4: watching a video or reviewing media, for the same reason `desk_shell`
        // is here - it runs commands in the persistent desk container and, for a
        // url, reaches the open internet from inside it.
        "watch_video",
        "review_media",
        // S10: an interactive shell session. Aliased onto `shell`'s own decision in
        // `get_permissions`, so it has to be tightened wherever `shell` is - the
        // same command, the same sandbox, just held open across turns.
        "shell_open",
        "shell_write",
        "shell_read",
        // S3: a bot's own repo checkout. `repo_read`/`repo_grep` are deliberately
        // NOT here - reading a checkout an unattended routine already has is the
        // same as `browse`/`read_page` staying out of this set. `repo_run` is,
        // despite defaulting to "allow" in chat: it is a shell inside that
        // checkout, and the whole reason `shell` is tightened is that nobody is
        // awake at 06:00 to watch what runs. `repo_edit`, `repo_branch` and
        // `repo_pr` follow it for the same reason `adopt_thread` is here - each one
        // changes something (a file, a ref, a pull request) that Josh did not see
        // happen.
        "repo_run",
        "repo_edit",
        "repo_branch",
        "repo_pr",
        // 🔴 W5: a routine cannot write the roster a new tool. Josh asked for this in
        // as many words - "a routine cannot propose" - and the reason is the set's
        // own: an unattended run at 06:00 has nobody to object, and what this tool
        // produces is not one action but a permanent new capability every bot then
        // has. `decide_call` already pins it to "ask" everywhere; this is the second
        // lock, on the trigger rather than on the tool.
        "propose_tool",
        // W9: an unattended run cannot hold `query_db` on "allow". Josh is not
        // awake to see WHICH row of CRM, GlassDex or Repull a 06:00 routine read,
        // and unlike the tools above it this one reaches live business data.
        "query_db",
        // S7: an unattended run cannot hold `purchase` on "allow" either, for the
        // same reason `propose_tool` is here - nobody is awake at 06:00 to answer
        // for real money leaving the building.
        "purchase",
        // `hire_bot` puts a permanent bot on the roster. An unattended run cannot
        // hold that on "allow" either - the same reasoning `propose_tool` gets: what
        // this tool produces outlives the run, and nobody is awake at 06:00 to see
        // who was just hired.
        "hire_bot",
    ]
}

/// Tools offered whatever a run's tool list is narrowed to.
///
/// A routine's `tools` field says what it may CALL, not what it may always
/// reach for regardless. Saying something, asking Josh a question, touching its
/// own memory, and its own checklist are all scoped to the bot itself - the
/// same reasoning `default_decisions` gives each of them for being "allow" by
/// default - so narrowing a routine's tool list to "just the live check" must
/// not also take away its voice.
pub fn always_on_set() -> Vec<&'static str> {
    vec![
        "say",
        "ask_josh",
        "remember",
        "search_memory",
        "add_task",
        "update_task",
        "list_tasks",
        "read_output",
    ]
}

/// S10: tool names that carry no permission row of their own and instead take
/// whatever `shell` resolves to. Applied at read time, not hardcoded in
/// `default_decisions`, because a default is fixed at import time and this has
/// to follow Josh's actual `shell` setting, allow, ask or deny, whatever it is
/// when asked.
const SHELL_SESSION_TOOLS: &[&str] = &["shell_open", "shell_write", "shell_read"];

/// Get the permissions for a bot, merging stored overrides over the defaults.
/// Shell aliases (`shell_open`, `shell_write`, `shell_read`) take the decision
/// for `shell` unless stored explicitly.
pub fn get_permissions(db: &store::Db, bot_id: &str) -> Result<Permissions, rusqlite::Error> {
    let row = db.conn().query_row(
        "SELECT permissions FROM bots WHERE id = ?",
        [bot_id],
        |row| row.get::<_, String>(0),
    );

    let mut stored: Permissions = HashMap::new();
    match row {
        Ok(json_str) => {
            if !json_str.is_empty() {
                if let Ok(parsed) = serde_json::from_str::<Value>(&json_str) {
                    if let Some(obj) = parsed.as_object() {
                        for (k, v) in obj.iter() {
                            if let Some(s) = v.as_str() {
                                if let Some(dec) = Decision::from_str(s) {
                                    stored.insert(k.clone(), dec);
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(e),
    }

    let mut merged = default_decisions();
    for (k, v) in stored.iter() {
        merged.insert(k.clone(), *v);
    }

    // Apply shell session aliases: if `shell_*` is not in stored, use the decision for `shell`
    let shell_decision = merged.get("shell").copied().unwrap_or(Decision::Ask);
    for &tool in SHELL_SESSION_TOOLS {
        if !stored.contains_key(tool) {
            merged.insert(tool.to_string(), shell_decision);
        }
    }

    Ok(merged)
}

/// Set the permissions for a bot, storing only valid decisions. Returns the
/// merged result (defaults + overrides).
pub fn set_permissions(
    db: &store::Db,
    bot_id: &str,
    permissions: &Permissions,
) -> Result<Permissions, rusqlite::Error> {
    let mut clean = HashMap::new();
    for (k, v) in permissions {
        clean.insert(k.clone(), v);
    }

    let json_str = serde_json::to_string(&clean).unwrap_or_else(|_| "{}".to_string());
    db.conn().execute(
        "UPDATE bots SET permissions = ? WHERE id = ?",
        [json_str, bot_id.to_string()],
    )?;

    get_permissions(db, bot_id)
}

/// The decision for a tool on a single bot. Defaults to "ask" if not found.
pub fn decide(db: &store::Db, bot_id: &str, tool_name: &str) -> Result<Decision, rusqlite::Error> {
    let perms = get_permissions(db, bot_id)?;
    Ok(perms.get(tool_name).copied().unwrap_or(Decision::Ask))
}

/// The decision accounting for the specific arguments (if the tool cares).
///
/// 🔴 `ask_josh` with `wait: true` has to park the run, and that is not a
/// permission Josh sets - it is the bot saying "I cannot go further without an
/// answer". Parking reuses the approval machinery whole, and "ask" is how a tool
/// asks to be parked, so the wait form returns "ask" whatever the stored map
/// says.
///
/// 🔴 The NOT-waiting form returns "allow" rather than the stored value on
/// purpose. If a future settings screen let `ask_josh` be set to "deny", a bot
/// that needed a decision would be unable to ask for one and would go back to
/// guessing - which is the entire failure this closes. Being asked a question is
/// not a privilege that needs revoking.
pub fn decide_call(base: Decision, tool_name: &str, args: &str) -> Decision {
    // 🔴 W5: `propose_tool` can be turned OFF and cannot be turned to "always".
    //
    // The approval IS the mechanism - approving the call is what moves the source
    // into `tools/approved` and offers it to the roster - so an "allow" in the
    // grid would not skip a prompt, it would remove the only human step between a
    // bot writing code and every bot running it. "Never" still works, and is the
    // setting for a bot that has no business writing tools.
    if tool_name == "propose_tool" {
        return if base == Decision::Deny {
            Decision::Deny
        } else {
            Decision::Ask
        };
    }

    // 🔴 S7: `purchase` gets the identical pin, for the identical reason -
    // "always" in the grid must not skip the one human step between a bot and
    // real money leaving the building. "Never" still works. `app.ts`'s
    // `runs.beforeAsk` is the second lock (re-pinning to "ask" even if a rule
    // somehow resolved it to "allow" first) and the monthly-limit check is the
    // one path that runs WITHOUT asking - and that path only ever REFUSES, it
    // never spends.
    if tool_name == "purchase" {
        return if base == Decision::Deny {
            Decision::Deny
        } else {
            Decision::Ask
        };
    }

    if tool_name != "ask_josh" {
        return base;
    }

    // Parse args for ask_josh: look for `wait: true`
    match args.trim() {
        "" => Decision::Allow,
        s => {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                if parsed.get("wait") == Some(&Value::Bool(true)) {
                    Decision::Ask
                } else {
                    Decision::Allow
                }
            } else {
                // Unparseable arguments cannot be a considered request to wait, and
                // stalling a run on malformed JSON is the worse of the two failures.
                Decision::Allow
            }
        }
    }
}

/// The permissions a run actually gets, given what started it.
///
/// 🔴 This can only ever SUBTRACT. `decide` still answers from one map and
/// takes no trigger, which is the property that makes a trigger-dependent
/// loophole structurally impossible - the comment above is the reason, and it
/// stands. What changed on 2026-09-11 is WHERE the tightening happens.
///
/// It used to happen by rewriting Josh's stored setting when a routine was
/// attached. That made the unattended case safe and the CHAT case unusable: a
/// bot with a routine could never hold `shell` on "allow", so every command he
/// asked for in conversation stopped for an approval, over and over, while he
/// was sitting right there. His words: *"It keeps prompting me for permissions
/// that it should have already had."* He was right, and he could not fix it -
/// the setting was being overwritten under him.
///
/// Now the stored value is his intent for a conversation, and an unattended run
/// gets a copy of it with the dangerous tools pulled back to "ask". Nobody is
/// awake at 06:00 to approve, so that is a stop, exactly as before.
pub fn permissions_for_run(
    db: &store::Db,
    bot_id: &str,
    trigger: Trigger,
) -> Result<Permissions, rusqlite::Error> {
    let stored = get_permissions(db, bot_id)?;

    if trigger == Trigger::Chat {
        return Ok(stored);
    }

    let mut tightened = stored.clone();
    let tighten_tools = tighten_set();
    for (tool, decision) in stored.iter() {
        if tighten_tools.contains(&tool.as_str()) && *decision == Decision::Allow {
            tightened.insert(tool.clone(), Decision::Ask);
        }
    }
    Ok(tightened)
}

/// Whether a tool is one of the dangerous ones an unattended run may never hold
/// on "allow" (see `tighten_set`).
///
/// Exported for `rules.ts` use: a natural-language rule that resolves to "allow"
/// must not lift one of these for anything but a `chat` trigger, or an
/// auto-review rule would reopen exactly the hole `permissions_for_run` closes -
/// a routine firing unattended and matching a rule Josh wrote for a
/// conversation he was watching.
pub fn cannot_be_lifted_unattended(tool_name: &str) -> bool {
    tighten_set().contains(&tool_name)
}
