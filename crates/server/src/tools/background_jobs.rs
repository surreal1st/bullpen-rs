//! Background job tools — shell and agent background work.
//! Port of `app.ts` tool specs and dispatch for job tools.

use std::sync::{Arc, Mutex};

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::{Db, describe_job, get_job, list_jobs};

use crate::delegate::{DEPTH_REFUSAL, MAX_DELEGATION_DEPTH, find_bot};
use crate::job_runner::{
    JobRunnerDeps, JobSandbox, await_job, start_agent_job, start_shell_job, stop_job,
};
use crate::tools::ColleagueAskHook;

pub fn ask_in_background_spec() -> ToolSpec {
    ToolSpec {
        name: "ask_in_background".to_string(),
        description: "Ask another bot something WITHOUT waiting for its answer. Use this instead of message_bot when you want two or three colleagues working at once, or when their answer is not needed before you can carry on. Read the answer later with job_status.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "bot": { "type": "string", "description": "The colleague's name." },
                "question": { "type": "string", "description": "What to ask. Include the context they need." }
            },
            "required": ["bot", "question"]
        }),
    }
}

pub fn run_in_background_spec() -> ToolSpec {
    ToolSpec {
        name: "run_in_background".to_string(),
        description: "Start a long shell command and get a job id back straight away, instead of waiting for it. Use it for anything that takes more than a few seconds - a build, a big download, a long script. It keeps running after your turn ends. Read the result later with job_status.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command." },
                "label": { "type": "string", "description": "A few words naming what it is, for the job list." },
                "notify_when": { "type": "string", "description": "A regular expression. When the job's output matches it, Josh is told once, in this chat, while the job keeps running." }
            },
            "required": ["command"]
        }),
    }
}

pub fn job_status_spec() -> ToolSpec {
    ToolSpec {
        name: "job_status".to_string(),
        description: "Check a background job. With no id it lists yours. A job that is still running says so - ask again on a later turn rather than waiting here.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id. Omit to list them all." }
            }
        }),
    }
}

pub fn await_job_spec() -> ToolSpec {
    ToolSpec {
        name: "await_job".to_string(),
        description: "Wait for a background job to finish, up to a timeout. Returns the result when done, or tells you to check again later if it's still running after the timeout.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id." },
                "seconds": { "type": "number", "description": "How long to wait, 1 to 60 seconds. Defaults to 30." }
            },
            "required": ["id"]
        }),
    }
}

pub fn stop_job_spec() -> ToolSpec {
    ToolSpec {
        name: "stop_job".to_string(),
        description: "Stop a background job you started.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The job id." }
            },
            "required": ["id"]
        }),
    }
}

#[derive(Deserialize, Default)]
struct AskInBackgroundArgs {
    #[serde(default)]
    bot: String,
    #[serde(default)]
    question: String,
}

#[derive(Deserialize, Default)]
struct RunInBackgroundArgs {
    #[serde(default)]
    command: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    notify_when: String,
}

#[derive(Deserialize, Default)]
struct JobIdArgs {
    #[serde(default)]
    id: String,
}

#[derive(Deserialize, Default)]
struct AwaitJobArgs {
    #[serde(default)]
    id: String,
    seconds: Option<f64>,
}

pub async fn run_ask_in_background(
    db: &Arc<Mutex<Db>>,
    bot_id: &str,
    delegation_depth: u32,
    colleague_ask: &ColleagueAskHook,
    args: &str,
) -> String {
    if delegation_depth >= MAX_DELEGATION_DEPTH {
        return DEPTH_REFUSAL.to_string();
    }
    let parsed: AskInBackgroundArgs = serde_json::from_str(args).unwrap_or_default();
    let question = parsed.question.trim();
    if question.is_empty() {
        return "Nothing was asked: the question was empty.".to_string();
    }
    let target = {
        let guard = super::lock_db(db);
        find_bot(&guard, parsed.bot.trim())
    };
    let Some((to_id, to_name)) = target else {
        let roster = {
            let guard = super::lock_db(db);
            store::list_bots(&guard, false)
                .unwrap_or_default()
                .into_iter()
                .map(|b| b.name)
                .collect::<Vec<_>>()
                .join(", ")
        };
        return format!("There is no bot called that. The roster is: {roster}");
    };
    if to_id == bot_id {
        return "That is you. Answer it yourself.".to_string();
    }

    let db = Arc::clone(db);
    let ask = Arc::clone(colleague_ask);
    start_agent_job(db, bot_id, &to_id, &to_name, question, move |to, q| {
        let ask = Arc::clone(&ask);
        ask(to, q)
    })
}

pub async fn run_run_in_background(
    db: &Arc<Mutex<Db>>,
    job_sandbox: &Arc<dyn JobSandbox>,
    bot_id: &str,
    args: &str,
) -> String {
    let parsed: RunInBackgroundArgs = serde_json::from_str(args).unwrap_or_default();
    let command = parsed.command.trim();
    if command.is_empty() {
        return "No command was given.".to_string();
    }
    let notify = if parsed.notify_when.trim().is_empty() {
        None
    } else {
        Some(parsed.notify_when.trim())
    };
    let deps = JobRunnerDeps::new(Arc::clone(db), Arc::clone(job_sandbox));
    start_shell_job(&deps, bot_id, parsed.label.trim(), command, notify).await
}

pub fn run_job_status(db: &Arc<Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let parsed: JobIdArgs = serde_json::from_str(args).unwrap_or_default();
    let id = parsed.id.trim();
    let db = super::lock_db(db);
    if id.is_empty() {
        let all = list_jobs(&db, bot_id);
        if all.is_empty() {
            return "You have no background jobs.".to_string();
        }
        return all
            .iter()
            .map(|j| format!("[{}] {} (id {})", j.status.as_str(), j.label, j.id))
            .collect::<Vec<_>>()
            .join("\n");
    }
    match get_job(&db, bot_id, id) {
        None => "No job of yours has that id.".to_string(),
        Some(job) => describe_job(&job),
    }
}

pub async fn run_await_job(
    db: &Arc<Mutex<Db>>,
    job_sandbox: &Arc<dyn JobSandbox>,
    bot_id: &str,
    args: &str,
) -> String {
    let parsed: AwaitJobArgs = serde_json::from_str(args).unwrap_or_default();
    let id = parsed.id.trim();
    if id.is_empty() {
        return "No job id was given.".to_string();
    }
    let seconds = parsed.seconds.map(|s| s.round() as u32).filter(|&s| s > 0);
    let deps = JobRunnerDeps::new(Arc::clone(db), Arc::clone(job_sandbox));
    await_job(&deps, bot_id, id, seconds).await
}

pub async fn run_stop_job(
    db: &Arc<Mutex<Db>>,
    job_sandbox: &Arc<dyn JobSandbox>,
    bot_id: &str,
    args: &str,
) -> String {
    let parsed: JobIdArgs = serde_json::from_str(args).unwrap_or_default();
    let id = parsed.id.trim();
    if id.is_empty() {
        return "No job id was given.".to_string();
    }
    let deps = JobRunnerDeps::new(Arc::clone(db), Arc::clone(job_sandbox));
    stop_job(&deps, bot_id, id).await
}
