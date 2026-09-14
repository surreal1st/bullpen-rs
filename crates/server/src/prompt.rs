//! Assembles the prompt. Port of `src/server/prompt.ts`'s `buildPrompt`,
//! `recentHistory`, `getHouseRules`/`setHouseRules`, and `app.ts`'s
//! `roomInstruction`/`withRoomInstruction`.
//!
//! Order is the whole point. Instructions and the memory core sit at the
//! very front and do not change between turns of a conversation, so a
//! provider can serve the prefix from cache. Anything that varies goes after
//! them, or the cache never hits and the core costs full price on every
//! message.

use model::ModelMessage;
use shared::Bot;
use shared::nothing_new::NOTHING_NEW;
use store::Db;

/// The facts about a bot's own situation, which it otherwise invents.
/// Verbatim from the TS `WHERE_YOU_ARE` (`prompt.ts:38-61`).
const WHERE_YOU_ARE: &str = r#"## Where you are

You run on meridian, a server in Josh's house. Three places hold files, and
they are not the same place:

1. `/work` - a directory that is yours alone, survives between runs, and has
   no network. Use the `shell` tool. Nothing of Josh's is in it.
2. His WORKSTATION (`C:\...` or `D:\...`). Use the `read_file` tool. It
   works through the Bullpen desktop app running on that machine, and it asks
   him to approve each read.
3. meridian itself (`/home/...`). Use the `ssh` tool. Also approved per use.

So a Windows path he types IS something you can reach - ask to read it with
`read_file` rather than telling him it is impossible. If you do not have the
tool, say which tool you would need.

Never ask him to paste a file's contents, create a directory for you, or drag
a file in as a workaround for a tool you have. Asking him to do your reading
is how a routine stops forever.

A file he attaches arrives in the conversation. Text and PDFs arrive as text
you can already read; images arrive as pictures you can look at."#;

/// How every bot writes, stated once for the whole roster. Verbatim from the
/// TS `HOW_YOU_WRITE` (`prompt.ts:82-107`).
const HOW_YOU_WRITE: &str = r#"## How to write to Josh

Lead with the answer. The first line is the result, the finding or the number
he asked for - never a restatement of the question and never what you are
about to do.

- No preamble. Not "I'll check that for you", not "Great question", not
  "Let me take a look". Do the thing and say what came back.
- No sign-off, no offers of further help, no summary of what you just said.
- Never narrate your method. He does not want to know which tool you used,
  how many calls it took, or that you batched anything, unless he asked or
  unless it changes what he should do next.
- Never apologise for how long something took and never report your own
  timing unless he asked for it.
- Say what you DID, in the past tense. Not what you are going to do.
- Bullets for lists of things. Prose only when it genuinely reads better.
- Plain words. If a short word works, it is the right word.

Length follows the question. A yes/no question gets a line; a real analysis
gets as much room as it needs. What never earns room is padding: throat
clearing, hedging, restating, or explaining yourself.

When you have nothing to report, say nothing. Silence is a complete answer to
a routine that found nothing."#;

/// How much conversation to replay. Roughly four characters to a token, and
/// this is deliberately a rough count - an exact tokeniser here would be a
/// dependency and a per-run cost to save nothing, because the budget is a
/// judgement call, not a limit anyone hits exactly.
const HISTORY_BUDGET_TOKENS: i64 = 6000;

/// One turn of conversation history. Narrower than `shared::Message` on
/// purpose: `recent_history` and `build_prompt` only care who spoke and what
/// they said.
#[derive(Debug, Clone)]
pub struct HistoryTurn {
    pub role: String,
    pub content: String,
}

impl HistoryTurn {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// Keeps the newest history that fits `HISTORY_BUDGET_TOKENS` (roughly 4
/// chars/token), oldest first. Mirrors the TS `recentHistory`: walks from
/// the newest message backward - the end of a conversation is the part that
/// matters - and always keeps at least one message even if it alone blows
/// the budget.
pub fn recent_history(history: &[HistoryTurn]) -> Vec<HistoryTurn> {
    let mut budget = HISTORY_BUDGET_TOKENS * 4;
    let mut kept: Vec<HistoryTurn> = Vec::new();

    for message in history.iter().rev() {
        // Count UTF-16 code units like TS `.length`, not UTF-8 bytes or Unicode scalars.
        // Emoji outside BMP count as 2 code units in UTF-16, 1 scalar in Rust.
        let utf16_len = message
            .content
            .chars()
            .map(|c| if c as u32 > 0xFFFF { 2 } else { 1 })
            .sum::<i64>();
        budget -= utf16_len;
        if budget < 0 && !kept.is_empty() {
            break;
        }
        kept.push(message.clone());
    }

    kept.reverse();
    kept
}

/// Josh's own rules, applied to every bot. Verbatim from the TS
/// `DEFAULT_RULES` (`prompt.ts:227-273`). `pub`: F13's `GET /api/rules`
/// echoes this back as `fallback` (`app.ts:2958`) so the rules textarea has
/// a real placeholder instead of reading as "no house rules exist".
pub const DEFAULT_RULES: &str = r#"1. Never claim what you have not verified this run. If you cannot verify
   something, say so in the same breath - unconfirmed, untested, did not
   check. A confident guess is worse than no answer.

2. Nothing ships below 90/100. Grade your own work with fresh eyes and no
   inflation before handing it over; below 90, redo it and say what changed.
   Rubric, out of 100:
     Verified 30 - every claim traces to something you did THIS run.
     Complete 20 - the whole ask, with anything you could not do named.
     Simple 20 - the laziest thing that works; say what you did not build.
     Looked at 15 - you observed the real result, not what it should be.
     Reversible 15 - nothing destroyed without a checked replacement.
   One unverified claim caps the total at 60, however good the rest is. An
   average lets a made-up fact be offset by tidy formatting; a cap does not.

3. Push back on every idea and every decision, including Josh's. You are an
   adversary and an operator. Adversary: try to make it fail, and name how it
   breaks. Operator: it is your time and your money on the line. Then answer
   with one synthesis of what you found. Never agree just to be agreeable.

4. Live facts over memory. Check the thing now. A remembered value is a lead,
   never evidence.

5. Do the laziest thing that actually works. Ask whether the task needs to
   exist at all, reach for what already exists before building, one line
   before fifty. Simple is the deliverable, not a compromise on it.

6. Never hand the work back to Josh. Hand it to a colleague, gladly - that
   is what the roster is for, and a lead fanning work out to the bot whose
   job it is beats doing it badly yourself. Handing it to JOSH is the one
   place it must not go.

   Exhaust the routes you hold first. A tool that failed once is not a wall,
   it is one route you have now tried: if a fetch comes back empty, open the
   page in your browser; if a browser call errors, try it again or by another
   road. Then ask the colleague whose area it is. Only when every route you
   hold and every colleague who could have it have ACTUALLY been tried do you
   say you could not - and then name each one and what it did, so he can see
   it was tried rather than declined.

   Someone needs to, you should, if you want me to take that over, I would
   rather you - none of those are results. Neither is asking permission for
   something you are already allowed to do: do it, then say what happened.
   Asking Josh to DECIDE something only he can decide is a different thing,
   and stays welcome."#;

const RULES_KEY: &str = "house_rules";
/// Same settings key the TS `getSharedCore`/`setSharedCore` use
/// (`import-grok.ts:370`) - "## About Josh", the shared core every bot's
/// prompt carries ahead of its own identity.
const SHARED_CORE_KEY: &str = "memory.shared_core";
/// Capped, because this rides in EVERY request for every bot. A rule nobody
/// trimmed is a bill nobody noticed.
const HOUSE_RULES_CHAR_CAP: usize = 4000;

/// Josh's house rules, or `DEFAULT_RULES` when he has not set any.
///
/// B12: called both from a request handler and from inside the spawned run
/// task (a room round, a tool call) with no `Result` to hand either one - a
/// store error here falls back to `DEFAULT_RULES` and logs, rather than
/// panicking a detached task (and, via the db mutex, every other request).
pub fn house_rules(db: &Db) -> String {
    db.settings_get(RULES_KEY)
        .unwrap_or_else(|err| {
            tracing::error!("house_rules: settings query failed: {err}");
            None
        })
        .unwrap_or_else(|| DEFAULT_RULES.to_string())
}

/// Stores Josh's house rules, capped at 4000 characters, and returns what
/// was actually stored. B12: a failed write is logged, not panicked - the
/// caller still gets back the (unsaved) cleaned value.
pub fn set_house_rules(db: &Db, rules: &str) -> String {
    let clean: String = rules.trim().chars().take(HOUSE_RULES_CHAR_CAP).collect();
    if let Err(err) = db.settings_set(RULES_KEY, &clean) {
        tracing::error!("set_house_rules: settings upsert failed: {err}");
    }
    clean
}

/// What rides after history when the conversation ends on a bot's own turn.
/// Verbatim from the TS `NO_NEW_MESSAGE_TRAILER` (`prompt.ts:310-313`) - a
/// prompt ending on an assistant turn is invalid to Anthropic and a silent
/// continuation trap for Gemini.
const NO_NEW_MESSAGE_TRAILER: &str = "Josh has not sent a new message - the conversation above ends on your own last turn. Do not continue or extend that reply as if it were unfinished. Say something new only if you have something worth saying unprompted; otherwise wait.";

/// Assembles the system prompt plus trimmed history for one bot's turn.
/// Block order (and this is the whole point - see the module doc): shared,
/// cacheable blocks first (`WHERE_YOU_ARE`, `HOW_YOU_WRITE`, house rules,
/// "## About Josh"), then per-bot blocks (identity, instructions, "## What
/// you already know", "## What you remember").
///
/// Skill index and open tasks/questions are left as hooks (S1-04 skips
/// them, per the ticket) rather than ported here.
pub fn build_prompt(db: &Db, bot: &Bot, history: &[HistoryTurn]) -> Vec<ModelMessage> {
    // B12: same posture as `house_rules` above - this runs inside the
    // spawned run task as often as it runs inside a request handler, so a
    // store error here degrades the prompt (an empty block) instead of
    // panicking either one.
    let shared_core = db
        .settings_get(SHARED_CORE_KEY)
        .unwrap_or_else(|err| {
            tracing::error!("build_prompt: shared core query failed: {err}");
            None
        })
        .unwrap_or_default();
    let shared_core = shared_core.trim();
    let core = store::get_core(db, &bot.id).unwrap_or_else(|err| {
        tracing::error!("build_prompt: memory core query failed: {err}");
        String::new()
    });
    let core = core.trim();

    let mut blocks: Vec<String> = vec![
        WHERE_YOU_ARE.to_string(),
        String::new(),
        HOW_YOU_WRITE.to_string(),
    ];

    let rules = house_rules(db);
    let rules = rules.trim();
    if !rules.is_empty() {
        blocks.push(String::new());
        blocks.push("## Rules for every bot".to_string());
        blocks.push(String::new());
        blocks.push(rules.to_string());
    }

    if !shared_core.is_empty() {
        blocks.push(String::new());
        blocks.push("## About Josh".to_string());
        blocks.push(shared_core.to_string());
    }

    // Who this bot is - name and purpose, which otherwise never reach the
    // model at all (see the TS doc comment on `identity`, prompt.ts:330-342).
    let name = bot.name.trim();
    let purpose = bot.purpose.trim();
    let mut identity: Vec<String> = Vec::new();
    if !name.is_empty() || !purpose.is_empty() {
        identity.push("## Who you are".to_string());
        identity.push(String::new());
        if name.is_empty() {
            identity.push(format!("Your job: {purpose}."));
        } else {
            identity.push(format!("You are **{name}**."));
            if !purpose.is_empty() {
                identity.push(format!("Your job: {purpose}."));
            }
        }
        identity.push(String::new());
    }

    blocks.push(String::new());
    blocks.extend(identity);
    blocks.push(bot.instructions.clone());

    if !core.is_empty() {
        blocks.push(String::new());
        blocks.push("## What you already know".to_string());
        blocks.push(core.to_string());
    }

    // Skill index: SKIP for now (hook for a later ticket).
    // Open tasks / questions: SKIP for now (hook for a later ticket).

    // S3-03: tiered recall - own > project > shared, newest first within
    // each tier, one header per tier so the model can tell whose fact is
    // whose. `store::recall_for` returns a flat, untagged `Vec<LogEntry>`
    // that cannot be split back into tiers by a caller, so this calls
    // `store::scoped_entries`/`count_scoped` directly, one per tier.
    if let Err(err) = store::sweep_expired(db) {
        tracing::error!("build_prompt: sweep_expired failed: {err}");
    }
    let projects = store::projects_for(db, &bot.id).unwrap_or_else(|err| {
        tracing::error!("build_prompt: projects_for query failed: {err}");
        Vec::new()
    });

    let mut candidate_tiers: Vec<(String, Vec<store::LogEntry>)> = Vec::new();
    let mut available: i64 = 0;
    let mut own_candidates: Vec<store::LogEntry> = Vec::new();

    match own_tier(db, &bot.id) {
        Ok((entries, total)) => {
            available += total;
            own_candidates = entries.clone();
            candidate_tiers.push(("## What you know".to_string(), entries));
        }
        Err(err) => tracing::error!("build_prompt: own recall query failed: {err}"),
    }
    for project in &projects {
        match project_tier(db, &project.id) {
            Ok((entries, total)) => {
                available += total;
                candidate_tiers.push((format!("## Project: {}", project.name), entries));
            }
            Err(err) => tracing::error!("build_prompt: project recall query failed: {err}"),
        }
    }
    match shared_tier(db) {
        Ok((entries, total)) => {
            available += total;
            candidate_tiers.push(("## Shared".to_string(), entries));
        }
        Err(err) => tracing::error!("build_prompt: shared recall query failed: {err}"),
    }

    // One running budget spent in precedence order - own gets first claim,
    // shared is first to be dropped when it is tight.
    let mut spent: i64 = 0;
    let mut kept_total: i64 = 0;
    let mut kept_tiers: Vec<(String, Vec<store::LogEntry>)> = Vec::new();
    for (header, entries) in candidate_tiers {
        let mut kept: Vec<store::LogEntry> = Vec::new();
        for entry in entries {
            let cost = approx_tokens(&entry.content) + 2;
            if spent + cost > store::RECALL_TOKEN_BUDGET {
                break;
            }
            spent += cost;
            kept.push(entry);
        }
        kept_total += kept.len() as i64;
        if !kept.is_empty() {
            // Newest first out of the query; a prompt reads better oldest first.
            kept.reverse();
            kept_tiers.push((header, kept));
        }
    }

    // Emit recall blocks if anything fits the budget, or clip the newest own
    // entry as a fallback when nothing does (a single entry can be bigger than
    // the whole budget). See store::recall_for's fallback comment.
    if !kept_tiers.is_empty() {
        for (header, entries) in &kept_tiers {
            blocks.push(String::new());
            blocks.push(header.clone());
            blocks.push(String::new());
            for entry in entries {
                blocks.push(format!("- {}", entry.content));
            }
        }
    } else if !own_candidates.is_empty() && available > 0 {
        // Fallback: nothing fits, but we have the bot's own entries. Clip the
        // newest one as store::recall_for does.
        let clip = (store::RECALL_TOKEN_BUDGET * 4).max(0) as usize;
        let mut clipped = own_candidates[0].clone();
        let truncated: String = clipped.content.chars().take(clip).collect();
        clipped.content = format!("{truncated}\n[...truncated, search_memory for the rest]");

        blocks.push(String::new());
        blocks.push("## What you know".to_string());
        blocks.push(String::new());
        blocks.push(format!("- {}", clipped.content));
        kept_total = 1;
    }

    // Emit the "older notes" line whenever entries exist beyond what was kept.
    let older = (available - kept_total).max(0);
    if older > 0 {
        blocks.push(String::new());
        blocks.push(format!(
            "There are {older} older notes not shown here. Use search_memory to look something up rather than telling Josh you do not know it."
        ));
    }

    let system = blocks.join("\n");

    // A prompt that ends on an assistant turn is invalid to Anthropic and a
    // silent continuation trap for Gemini - see NO_NEW_MESSAGE_TRAILER above.
    let mut trimmed: Vec<ModelMessage> = recent_history(history)
        .into_iter()
        .map(|turn| {
            if turn.role == "assistant" {
                ModelMessage {
                    role: "assistant".to_string(),
                    content: model::MessageContent::Text(turn.content),
                    tool_calls: None,
                    tool_call_id: None,
                }
            } else {
                ModelMessage::user(turn.content)
            }
        })
        .collect();

    if trimmed.last().map(|m| m.role.as_str()) == Some("assistant") {
        trimmed.push(ModelMessage::user(NO_NEW_MESSAGE_TRAILER));
    }

    let mut messages = vec![ModelMessage::system(system)];
    messages.extend(trimmed);
    messages
}

/// How many recent entries a single tier pulls before the budget trims it
/// further. Matches the cap `store::scoped_entries` uses internally.
const TIER_QUERY_LIMIT: i64 = 40;

/// The bot's own-scope log, newest first, plus how many exist in total
/// (ignoring the query limit) so `build_prompt` can report an accurate
/// "older" count.
fn own_tier(db: &Db, bot_id: &str) -> rusqlite::Result<(Vec<store::LogEntry>, i64)> {
    let entries = store::scoped_entries(db, bot_id, store::Scope::Own, &[], TIER_QUERY_LIMIT)?;
    let total = store::count_scoped(db, bot_id, store::Scope::Own, &[])?;
    Ok((entries, total))
}

/// One project's log, newest first, plus its total count.
fn project_tier(db: &Db, project_id: &str) -> rusqlite::Result<(Vec<store::LogEntry>, i64)> {
    let ids = [project_id.to_string()];
    let entries = store::scoped_entries(db, "", store::Scope::Project, &ids, TIER_QUERY_LIMIT)?;
    let total = store::count_scoped(db, "", store::Scope::Project, &ids)?;
    Ok((entries, total))
}

/// The shared log every bot can see, newest first, plus its total count.
fn shared_tier(db: &Db) -> rusqlite::Result<(Vec<store::LogEntry>, i64)> {
    let entries = store::scoped_entries(db, "", store::Scope::Shared, &[], TIER_QUERY_LIMIT)?;
    let total = store::count_scoped(db, "", store::Scope::Shared, &[])?;
    Ok((entries, total))
}

/// Roughly four characters per token. Mirrors `store::memory`'s private
/// `approx_tokens` - enough to police the recall budget, not a billing
/// system.
fn approx_tokens(text: &str) -> i64 {
    (text.len() as i64 + 3) / 4
}

/// What a room member is told before it speaks. `mandatory` (Josh typed
/// "@everyone") lifts only the silence option: everyone still answers in a
/// few sentences, nobody gets to skip the turn. Mirrors the TS
/// `roomInstruction` (`app.ts:817-827`).
pub fn room_instruction(
    db: &Db,
    all_bot_ids: &[String],
    speaking_id: &str,
    mandatory: bool,
) -> String {
    let mut names: Vec<String> = Vec::new();
    for id in all_bot_ids.iter().filter(|id| id.as_str() != speaking_id) {
        let name = store::get_bot(db, id)
            .unwrap_or_else(|err| {
                tracing::error!("room_instruction: bot query failed: {err}");
                None
            })
            .map(|bot| bot.name)
            .unwrap_or_else(|| id.clone());
        names.push(name);
    }
    let with_whom = format!("You are in a room with {}.", names.join(", "));

    if mandatory {
        format!(
            "{with_whom} Josh asked everyone to weigh in with @everyone - reply, in 1 to 3 sentences, without repeating what someone else already said."
        )
    } else {
        format!(
            "{with_whom} Reply in 1 to 3 sentences, and only if you have something the others have not already said - do not repeat them or restate the question. If you have nothing to add, reply with exactly {NOTHING_NEW} and say nothing else; staying quiet is normal here and keeps the room from thrashing."
        )
    }
}

/// Appends the room instruction as its OWN trailing "user" turn rather than
/// folding it into an existing message, so the prompt ends on "user" no
/// matter what shape the history in front of it has. Mirrors the TS
/// `withRoomInstruction` (`app.ts:841-844`).
pub fn with_room_instruction(mut messages: Vec<ModelMessage>, note: &str) -> Vec<ModelMessage> {
    messages.push(ModelMessage::user(note));
    messages
}
