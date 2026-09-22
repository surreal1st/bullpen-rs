//! S12-02: `deliver` tool — real files into the library.

use std::sync::Arc;

use model::ToolSpec;
use store::{Db, NewMessage, get_or_create_conversation};

use crate::deliverables::{DeliverDeps, DockerDeskRunner, deliver_file};
use crate::desk;
use crate::observations::{DesktopStateRegistry, ObservationRegistry};

pub struct DeliverRunEnv {
    pub vm_docker: Arc<dyn crate::vm::DockerRun>,
    pub vm_config: Arc<store::vms::VmConfig>,
    pub vm_enabled: bool,
    pub desktop_states: Arc<DesktopStateRegistry>,
    pub observations: Arc<ObservationRegistry>,
}

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "deliver".to_string(),
        description: "Produce a REAL file - a PDF, a spreadsheet, a slide deck, a video clip, or plain text - and post it as an attachment in this chat and in the Library. Use this instead of describing what a document would contain: Josh gets something to open. pdf prints HTML through the shared browser. spreadsheet builds a real .xlsx from a JSON table. deck builds a real .pptx, one slide per heading in a plain-text outline. clip cuts a real .mp4 with ffmpeg from a file already on the desk, under /work or /workspace/.recordings - never an arbitrary path. text writes markdown/txt/csv straight through.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["pdf", "spreadsheet", "deck", "clip", "text"],
                    "description": "What to produce."
                },
                "name": { "type": "string", "description": "File name. The right extension is added if missing." },
                "content": {
                    "description": "pdf: the HTML to print. spreadsheet: a JSON table. deck: plain-text outline. text: raw content. Not used for clip."
                },
                "path": {
                    "type": "string",
                    "description": "clip only: source video path under /work or /workspace/.recordings."
                },
                "from": { "type": "number", "description": "clip only: start second. Default 0." },
                "to": { "type": "number", "description": "clip only: end second." }
            },
            "required": ["kind", "name"]
        }),
    }
}

pub async fn run(
    db: &Arc<std::sync::Mutex<Db>>,
    data_dir: &str,
    bot_id: &str,
    args: &str,
    env: &DeliverRunEnv,
) -> String {
    let input: serde_json::Value =
        serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));
    let kind = input.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let cdp = if kind == "pdf" && env.vm_enabled {
        Some(
            desk::cdp_for_bot(
                db,
                Arc::clone(&env.vm_docker),
                &env.vm_config,
                env.vm_enabled,
                bot_id,
            )
            .await,
        )
    } else {
        None
    };

    let mut desktop_guard = None;
    let runner: Option<Arc<dyn crate::deliverables::DeskRunner>> =
        if kind == "clip" && env.vm_enabled {
            match desk::desk_config_for_bot_owned(
                Arc::clone(db),
                Arc::clone(&env.vm_docker),
                Arc::clone(&env.vm_config),
                env.vm_enabled,
                bot_id.to_string(),
                Arc::clone(&env.desktop_states),
                Arc::clone(&env.observations),
            )
            .await
            {
                Ok((cfg, guard)) => {
                    let r = Arc::new(DockerDeskRunner::new(Arc::clone(&env.vm_docker), &cfg))
                        as Arc<dyn crate::deliverables::DeskRunner>;
                    desktop_guard = Some(guard);
                    Some(r)
                }
                Err(_) => None,
            }
        } else {
            None
        };

    let outcome = deliver_file(
        DeliverDeps {
            db: Arc::clone(db),
            data_dir: data_dir.to_string(),
            bot_id: bot_id.to_string(),
            cdp,
            runner,
        },
        &input,
    )
    .await;
    drop(desktop_guard);

    if !outcome.ok {
        return outcome.detail;
    }
    let Some(attachment) = outcome.attachment else {
        return outcome.detail;
    };

    let guard = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conv = match get_or_create_conversation(&guard, bot_id) {
        Ok(id) => id,
        Err(_) => {
            return format!(
                "Delivered, but could not post into the chat. {}",
                outcome.detail
            );
        }
    };
    let bot_id_opt = Some(bot_id.to_string());
    if let Err(err) = store::append_message(
        &guard,
        &conv,
        "assistant",
        &format!("Delivered: {}", attachment.name),
        NewMessage {
            model: None,
            error: None,
            attachment_id: Some(attachment.id.clone()),
            bot_id: bot_id_opt,
            usage: None,
        },
    ) {
        return format!("Delivered, but could not post into the chat: {err}");
    }
    drop(guard);

    format!(
        "Delivered, and posted into the chat and the Library. {}",
        outcome.detail
    )
}
