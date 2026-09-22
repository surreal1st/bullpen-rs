//! W4a deliverables — port of `deliverables.ts`.

mod deck;
mod spreadsheet;
mod zip;

pub use deck::{DeckSlide, PPTX_CONTENT_TYPE, build_pptx, parse_outline};
pub use spreadsheet::{XLSX_CONTENT_TYPE, build_xlsx, coerce_table, col_letter};
pub use zip::{build_zip, crc32};

use std::sync::{Arc, Mutex, PoisonError};

use store::{Attachment, Db, StoreAttachmentInput, store_attachment};
use uuid::Uuid;

use crate::desk::{self, Cdp};
use crate::scope::scope_for_bot;

pub const DELIVER_KINDS: [&str; 5] = ["pdf", "spreadsheet", "deck", "clip", "text"];
pub const MAX_CLIP_SECONDS: i64 = 10 * 60;

#[derive(Debug, Clone)]
pub struct DeliverOutcome {
    pub ok: bool,
    pub attachment: Option<Attachment>,
    pub detail: String,
}

#[derive(Clone)]
pub struct DeliverDeps {
    pub db: Arc<Mutex<Db>>,
    pub data_dir: String,
    pub bot_id: String,
    pub cdp: Option<Arc<dyn Cdp>>,
    pub runner: Option<Arc<dyn DeskRunner>>,
}

fn lock_db(db: &Arc<Mutex<Db>>) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait::async_trait]
pub trait DeskRunner: Send + Sync {
    async fn run(&self, cmd: &str, args: &[&str], timeout_ms: u64) -> (bool, String);
    async fn read_file_base64(&self, path: &str, timeout_ms: u64) -> Option<Vec<u8>>;
}

pub struct DockerDeskRunner {
    docker: Arc<dyn crate::vm::DockerRun>,
    container: String,
}

impl DockerDeskRunner {
    pub fn new(docker: Arc<dyn crate::vm::DockerRun>, config: &desk::DeskConfig) -> Self {
        Self {
            docker,
            container: config.container.clone(),
        }
    }

    async fn exec(&self, argv: &[&str], timeout_ms: u64) -> (bool, String) {
        let mut full: Vec<&str> = vec!["exec", "-u", "abc", "-e", "HOME=/config", &self.container];
        full.extend(argv);
        let result = self.docker.call(&full, timeout_ms).await;
        (result.ok, format!("{}{}", result.stdout, result.stderr))
    }
}

#[async_trait::async_trait]
impl DeskRunner for DockerDeskRunner {
    async fn run(&self, cmd: &str, args: &[&str], timeout_ms: u64) -> (bool, String) {
        let mut argv = vec![cmd];
        argv.extend(args);
        self.exec(&argv, timeout_ms).await
    }

    async fn read_file_base64(&self, path: &str, timeout_ms: u64) -> Option<Vec<u8>> {
        let (ok, out) = self.exec(&["base64", "-w0", path], timeout_ms).await;
        if !ok {
            return None;
        }
        let trimmed: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(trimmed.as_bytes())
            .ok()
    }
}

pub fn resolve_clip_source(raw: &str) -> Result<String, String> {
    let path = raw.trim();
    if path.is_empty() {
        return Err("No source path was given.".into());
    }
    if path.contains("..") {
        return Err(r#"The source path may not contain ".."."#.into());
    }
    if path == "/work" || path.starts_with("/work/") {
        return Ok(path.to_string());
    }
    if path.starts_with("/workspace/.recordings/") {
        return Ok(path.to_string());
    }
    Err(
        "The source must be a file already on the desk, under /work or /workspace/.recordings - not an arbitrary path.".into(),
    )
}

pub async fn cut_clip(
    runner: &dyn DeskRunner,
    raw_path: &str,
    from_raw: Option<f64>,
    to_raw: Option<f64>,
) -> Result<Vec<u8>, String> {
    let validated = resolve_clip_source(raw_path)?;

    let from = from_raw
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.floor() as i64)
        .unwrap_or(0);
    let requested_to = to_raw
        .filter(|n| n.is_finite() && *n > from as f64)
        .map(|n| n.floor() as i64)
        .unwrap_or(from + 15);
    let to = requested_to.min(from + MAX_CLIP_SECONDS);

    let root = if validated == "/work" || validated.starts_with("/work/") {
        "/work"
    } else {
        "/workspace"
    };
    let tmp = format!("{root}/.deliver-tmp/{}", Uuid::new_v4());
    let out_path = format!("{tmp}/clip.mp4");

    runner.run("mkdir", &["-p", &tmp], 30_000).await;
    let from_s = from.to_string();
    let to_s = to.to_string();
    let (ok, output) = runner
        .run(
            "ffmpeg",
            &[
                "-nostdin",
                "-y",
                "-ss",
                &from_s,
                "-to",
                &to_s,
                "-i",
                &validated,
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                &out_path,
            ],
            120_000,
        )
        .await;

    let bytes = runner.read_file_base64(&out_path, 120_000).await;
    match bytes.filter(|b| !b.is_empty()) {
        Some(b) if ok => Ok(b),
        _ => Err(format!(
            "The clip could not be produced. ffmpeg said: {}",
            output
                .chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        )),
    }
}

pub async fn print_pdf(
    db: &Arc<Mutex<Db>>,
    cdp: &dyn Cdp,
    bot_id: &str,
    html: &str,
    settle_ms: u64,
) -> Option<Vec<u8>> {
    let existing = {
        let guard = lock_db(db);
        desk::existing_window(&guard, bot_id).ok()?
    };
    let target_id = if let Some(id) = existing {
        if cdp.has_target(&id).await {
            id
        } else {
            let id = cdp.create_window("about:blank").await.ok()?;
            {
                let guard = lock_db(db);
                desk::save_window(&guard, bot_id, &id).ok()?;
            }
            id
        }
    } else {
        let id = cdp.create_window("about:blank").await.ok()?;
        {
            let guard = lock_db(db);
            desk::save_window(&guard, bot_id, &id).ok()?;
        }
        id
    };
    cdp.call(&target_id, "Page.enable", serde_json::json!({}))
        .await
        .ok()?;
    let encoded: String = html
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let data_url = format!("data:text/html;charset=utf-8,{encoded}");
    cdp.call(
        &target_id,
        "Page.navigate",
        serde_json::json!({ "url": data_url }),
    )
    .await
    .ok()?;
    if settle_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
    }
    let reply = cdp
        .call(
            &target_id,
            "Page.printToPDF",
            serde_json::json!({ "printBackground": true }),
        )
        .await
        .ok()?;
    let data = reply.get("data").and_then(|v| v.as_str())?;
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .ok()
}

fn ensure_ext(raw_name: &str, ext: &str) -> String {
    let clean = raw_name.trim();
    let name = if clean.is_empty() {
        format!("deliverable.{ext}")
    } else {
        clean.to_string()
    };
    if name.to_lowercase().ends_with(&format!(".{ext}")) {
        name
    } else {
        format!("{name}.{ext}")
    }
}

fn text_content_type(name: &str) -> &str {
    let lower = name.to_lowercase();
    if lower.ends_with(".csv") {
        "text/csv"
    } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
        "text/markdown"
    } else {
        "text/plain"
    }
}

fn finish(
    deps: &DeliverDeps,
    name: String,
    content_type: &str,
    data: Vec<u8>,
    kind: &str,
    detail: String,
) -> DeliverOutcome {
    let guard = lock_db(&deps.db);
    let scope = scope_for_bot(&guard, &deps.bot_id).ok();
    let user_id = scope.as_ref().map(|s| s.user_id.as_str());
    match store_attachment(
        &guard,
        &deps.data_dir,
        StoreAttachmentInput {
            name: &name,
            content_type,
            data: &data,
            bot_id: Some(deps.bot_id.as_str()),
            kind: Some(kind),
            user_id,
        },
    ) {
        Ok(attachment) => DeliverOutcome {
            ok: true,
            attachment: Some(attachment),
            detail,
        },
        Err(store::StoreAttachmentError::Empty) => DeliverOutcome {
            ok: false,
            attachment: None,
            detail: "That file is empty.".into(),
        },
        Err(store::StoreAttachmentError::TooLarge { .. }) => DeliverOutcome {
            ok: false,
            attachment: None,
            detail: "That file exceeds the attachment size limit.".into(),
        },
        Err(store::StoreAttachmentError::Store(_)) => DeliverOutcome {
            ok: false,
            attachment: None,
            detail: "The file could not be saved.".into(),
        },
    }
}

pub async fn deliver_file(deps: DeliverDeps, input: &serde_json::Value) -> DeliverOutcome {
    let kind_raw = input.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if !DELIVER_KINDS.contains(&kind_raw) {
        return DeliverOutcome {
            ok: false,
            attachment: None,
            detail: format!(
                "\"{kind_raw}\" is not a kind deliver knows. Use one of: {}.",
                DELIVER_KINDS.join(", ")
            ),
        };
    }
    let raw_name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if raw_name.trim().is_empty() {
        return DeliverOutcome {
            ok: false,
            attachment: None,
            detail: "Give the file a name.".into(),
        };
    }

    match kind_raw {
        "text" => {
            let Some(content) = input.get("content").and_then(|v| v.as_str()) else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "text delivery needs content: the raw md/txt/csv text.".into(),
                };
            };
            if content.is_empty() {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "text delivery needs content: the raw md/txt/csv text.".into(),
                };
            }
            finish(
                &deps,
                raw_name.to_string(),
                text_content_type(raw_name),
                content.as_bytes().to_vec(),
                "text",
                format!("Wrote {raw_name}."),
            )
        }
        "spreadsheet" => {
            let Some(content) = input.get("content") else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "spreadsheet delivery needs content as a JSON table: an array of rows, each row an array of strings or numbers.".into(),
                };
            };
            let Some(table) = coerce_table(content) else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "spreadsheet delivery needs content as a JSON table: an array of rows, each row an array of strings or numbers.".into(),
                };
            };
            if table.is_empty() {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "spreadsheet delivery needs content as a JSON table: an array of rows, each row an array of strings or numbers.".into(),
                };
            }
            let name = ensure_ext(raw_name, "xlsx");
            let data = build_xlsx(&table);
            finish(
                &deps,
                name.clone(),
                XLSX_CONTENT_TYPE,
                data,
                "spreadsheet",
                format!("Built {name} with {} row(s).", table.len()),
            )
        }
        "deck" => {
            let Some(content) = input.get("content").and_then(|v| v.as_str()) else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "deck delivery needs content: a plain-text outline, one '# Heading' per slide.".into(),
                };
            };
            if content.trim().is_empty() {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "deck delivery needs content: a plain-text outline, one '# Heading' per slide.".into(),
                };
            }
            let slides = parse_outline(content);
            if slides.is_empty() {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "That outline produced no slides.".into(),
                };
            }
            let name = ensure_ext(raw_name, "pptx");
            let data = build_pptx(&slides);
            finish(
                &deps,
                name.clone(),
                PPTX_CONTENT_TYPE,
                data,
                "deck",
                format!("Built {name} with {} slide(s).", slides.len()),
            )
        }
        "pdf" => {
            let Some(content) = input.get("content").and_then(|v| v.as_str()) else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "pdf delivery needs content: the HTML to print.".into(),
                };
            };
            if content.trim().is_empty() {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "pdf delivery needs content: the HTML to print.".into(),
                };
            }
            let Some(cdp) = deps.cdp.as_deref() else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "No desk is available to print from.".into(),
                };
            };
            let name = ensure_ext(raw_name, "pdf");
            let Some(buffer) = print_pdf(&deps.db, cdp, &deps.bot_id, content, 400).await else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "The desk browser did not return a PDF.".into(),
                };
            };
            finish(
                &deps,
                name.clone(),
                "application/pdf",
                buffer,
                "pdf",
                format!("Printed {name}."),
            )
        }
        "clip" => {
            let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "clip delivery needs a path: the source video already on the desk."
                        .into(),
                };
            };
            if path.trim().is_empty() {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "clip delivery needs a path: the source video already on the desk."
                        .into(),
                };
            }
            let Some(runner) = deps.runner.as_deref() else {
                return DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail: "No desk is available to cut from.".into(),
                };
            };
            let from = input.get("from").and_then(|v| v.as_f64());
            let to = input.get("to").and_then(|v| v.as_f64());
            match cut_clip(runner, path, from, to).await {
                Ok(buffer) => {
                    let name = ensure_ext(raw_name, "mp4");
                    finish(
                        &deps,
                        name.clone(),
                        "video/mp4",
                        buffer,
                        "clip",
                        format!("Cut clip from {path}."),
                    )
                }
                Err(detail) => DeliverOutcome {
                    ok: false,
                    attachment: None,
                    detail,
                },
            }
        }
        _ => DeliverOutcome {
            ok: false,
            attachment: None,
            detail: "Unknown deliver kind.".into(),
        },
    }
}
