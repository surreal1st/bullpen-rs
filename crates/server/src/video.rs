//! S12-05: `watch_video` / `review_media` — port of `video.ts`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use model::{
    ContentPart, MessageContent, ModelEvent, ModelPort, ModelRequest, ModelUsage,
    VISION_DEFAULT_MODEL, utility_messages,
};
use store::{Db, get_attachment, read_attachment};
use uuid::Uuid;

use crate::deliverables::resolve_clip_source;
use crate::desk::{self, DeskConfig};
use crate::egress::Resolver;
use crate::transcribe::{self, TranscribeOptions};
use crate::vm;

pub const RECORDING_DIR: &str = "/workspace/.recordings";
pub const MAX_VIDEO_SECONDS: i64 = 30 * 60;
pub const MAX_FRAMES: i64 = 24;

const SCENE_THRESHOLD: f32 = 0.3;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: i64,
    pub to: i64,
}

pub fn bound_window(from_raw: Option<f64>, to_raw: Option<f64>) -> Window {
    let from = from_raw
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.floor() as i64)
        .unwrap_or(0);
    let requested_to = to_raw
        .filter(|n| n.is_finite() && *n > from as f64)
        .map(|n| n.floor() as i64)
        .unwrap_or(from + MAX_VIDEO_SECONDS);
    let to = requested_to.min(from + MAX_VIDEO_SECONDS);
    Window { from, to }
}

pub fn fmt_time(seconds: i64) -> String {
    let s = seconds.max(0);
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    let ss = format!("{sec:02}");
    if h > 0 {
        format!("{h}:{m:02}:{ss}")
    } else {
        format!("{m}:{ss}")
    }
}

pub fn parse_showinfo_times(output: &str) -> Vec<f64> {
    let mut times = Vec::new();
    for line in output.lines() {
        if let Some(idx) = line.find("pts_time:") {
            let rest = &line[idx + "pts_time:".len()..];
            if let Some(end) = rest.find(|c: char| !c.is_ascii_digit() && c != '.') {
                if let Ok(t) = rest[..end].parse::<f64>() {
                    times.push(t);
                }
            } else if let Ok(t) = rest.parse::<f64>() {
                times.push(t);
            }
        }
    }
    times
}

pub fn build_frame_lines(times: &[f64], model_reply: &str) -> Vec<String> {
    let raw_lines: Vec<String> = model_reply
        .lines()
        .map(|l| {
            let trimmed = l.trim();
            trimmed
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(|c: char| {
                    c == '.' || c == ')' || c == ':' || c == '-' || c == ' '
                })
                .trim()
                .to_string()
        })
        .filter(|l| !l.is_empty())
        .collect();
    times
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let line = raw_lines
                .get(i)
                .cloned()
                .unwrap_or_else(|| "(no description)".into());
            format!("[{}] {line}", fmt_time(t.floor() as i64))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start_sec: f64,
    pub end_sec: f64,
    pub text: String,
}

fn parse_vtt_timestamp(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let parts: Vec<&str> = raw.split(':').collect();
    let (h, m, s_part): (f64, f64, &str) = match parts.len() {
        3 => (
            parts[0].parse::<f64>().ok()?,
            parts[1].parse::<f64>().ok()?,
            parts[2],
        ),
        2 => (0.0, parts[0].parse::<f64>().ok()?, parts[1]),
        _ => return None,
    };
    let (sec, frac) = if let Some((s, ms)) = s_part.split_once(',') {
        (s.parse::<f64>().ok()?, ms.parse::<f64>().ok()? / 1000.0)
    } else if let Some((s, ms)) = s_part.split_once('.') {
        (s.parse::<f64>().ok()?, ms.parse::<f64>().ok()? / 1000.0)
    } else {
        (s_part.parse::<f64>().ok()?, 0.0)
    };
    Some(h * 3600.0 + m * 60.0 + sec + frac)
}

pub fn parse_vtt(text: &str) -> Vec<Cue> {
    let lines: Vec<&str> = text.lines().collect();
    let mut cues = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if !line.contains("-->") {
            i += 1;
            continue;
        }
        let mut split = line.split("-->");
        let start_raw = split.next().unwrap_or("").trim();
        let end_part = split.next().unwrap_or("").trim();
        let end_raw = end_part.split_whitespace().next().unwrap_or("");
        let Some(start_sec) = parse_vtt_timestamp(start_raw) else {
            i += 1;
            continue;
        };
        let Some(end_sec) = parse_vtt_timestamp(end_raw) else {
            i += 1;
            continue;
        };
        let mut text_lines = Vec::new();
        i += 1;
        while i < lines.len() && !lines[i].trim().is_empty() {
            let mut t = lines[i].to_string();
            while let Some(start) = t.find('<') {
                if let Some(end) = t[start..].find('>') {
                    t.replace_range(start..start + end + 1, "");
                } else {
                    break;
                }
            }
            text_lines.push(t);
            i += 1;
        }
        let cue_text = text_lines
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !cue_text.is_empty() {
            cues.push(Cue {
                start_sec,
                end_sec,
                text: cue_text,
            });
        }
    }
    cues
}

pub fn transcript_from_cues(cues: &[Cue], window: Window) -> String {
    let mut lines = Vec::new();
    let mut last = String::new();
    for cue in cues {
        if cue.end_sec < window.from as f64 || cue.start_sec > window.to as f64 {
            continue;
        }
        if cue.text == last {
            continue;
        }
        lines.push(format!(
            "[{}] {}",
            fmt_time(cue.start_sec.floor() as i64),
            cue.text
        ));
        last = cue.text.clone();
    }
    lines.join("\n")
}

pub fn resolve_recording_path(raw: &str) -> Result<String, String> {
    let resolved = resolve_clip_source(raw)?;
    if resolved != RECORDING_DIR && !resolved.starts_with(&format!("{RECORDING_DIR}/")) {
        return Err(format!(
            "The path must be a recording under {RECORDING_DIR}, not /work."
        ));
    }
    Ok(resolved)
}

fn image_mime_for_path(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".gif") {
        Some("image/gif")
    } else if lower.ends_with(".webp") {
        Some("image/webp")
    } else {
        None
    }
}

fn is_image_content_type(content_type: &str) -> bool {
    content_type.to_lowercase().starts_with("image/")
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub ok: bool,
    pub output: String,
}

#[async_trait]
pub trait VideoRunner: Send + Sync {
    async fn run(&self, cmd: &str, args: &[&str], timeout_ms: u64) -> RunResult;
    async fn read_file(&self, path: &str, timeout_ms: u64) -> Option<Vec<u8>>;
    async fn copy_in(&self, host_path: &str, dest_path: &str) -> bool;
}

pub struct DockerVideoRunner {
    docker: Arc<dyn vm::DockerRun>,
    container: String,
}

impl DockerVideoRunner {
    pub fn new(docker: Arc<dyn vm::DockerRun>, config: &DeskConfig) -> Self {
        Self {
            docker,
            container: config.container.clone(),
        }
    }

    async fn exec(&self, argv: &[&str], timeout_ms: u64) -> RunResult {
        let mut full = vec!["exec", "-u", "abc", "-e", "HOME=/config", &self.container];
        full.extend(argv);
        let result = self.docker.call(&full, timeout_ms).await;
        RunResult {
            ok: result.ok,
            output: format!("{}{}", result.stdout, result.stderr),
        }
    }
}

#[async_trait]
impl VideoRunner for DockerVideoRunner {
    async fn run(&self, cmd: &str, args: &[&str], timeout_ms: u64) -> RunResult {
        let mut argv = vec![cmd];
        argv.extend(args);
        self.exec(&argv, timeout_ms).await
    }

    async fn read_file(&self, path: &str, timeout_ms: u64) -> Option<Vec<u8>> {
        let result = self.exec(&["base64", "-w0", path], timeout_ms).await;
        if !result.ok {
            return None;
        }
        let trimmed: String = result
            .output
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(trimmed.as_bytes())
            .ok()
    }

    async fn copy_in(&self, host_path: &str, dest_path: &str) -> bool {
        let dest = format!("{}:{}", self.container, dest_path);
        self.docker
            .call(&["cp", "-L", host_path, &dest], 30_000)
            .await
            .ok
    }
}

fn tmp_dir() -> String {
    format!("/workspace/.video-tmp/{}", Uuid::new_v4())
}

pub async fn extract_frames(
    runner: &dyn VideoRunner,
    tmp: &str,
    input_path: &str,
    window: Window,
) -> (Vec<String>, Vec<f64>) {
    let _ = runner.run("mkdir", &["-p", tmp], DEFAULT_TIMEOUT_MS).await;
    let result = runner
        .run(
            "ffmpeg",
            &[
                "-nostdin",
                "-y",
                "-ss",
                &window.from.to_string(),
                "-to",
                &window.to.to_string(),
                "-i",
                input_path,
                "-vf",
                &format!("select='gt(scene,{SCENE_THRESHOLD})',showinfo"),
                "-vsync",
                "vfr",
                "-frames:v",
                &MAX_FRAMES.to_string(),
                &format!("{tmp}/frame_%03d.png"),
            ],
            DEFAULT_TIMEOUT_MS,
        )
        .await;
    let mut times = parse_showinfo_times(&result.output);
    times.truncate(MAX_FRAMES as usize);
    let mut frames = Vec::new();
    for i in 1..=times.len() {
        let path = format!("{tmp}/frame_{i:03}.png");
        if let Some(bytes) = runner.read_file(&path, DEFAULT_TIMEOUT_MS).await {
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            frames.push(format!("data:image/png;base64,{b64}"));
        }
    }
    times.truncate(frames.len());
    (frames, times)
}

pub async fn extract_audio(
    runner: &dyn VideoRunner,
    tmp: &str,
    input_path: &str,
    window: Window,
) -> Option<Vec<u8>> {
    let _ = runner.run("mkdir", &["-p", tmp], DEFAULT_TIMEOUT_MS).await;
    let _ = runner
        .run(
            "ffmpeg",
            &[
                "-nostdin",
                "-y",
                "-ss",
                &window.from.to_string(),
                "-to",
                &window.to.to_string(),
                "-i",
                input_path,
                "-vn",
                "-ac",
                "1",
                "-ar",
                "16000",
                &format!("{tmp}/audio.mp3"),
            ],
            DEFAULT_TIMEOUT_MS,
        )
        .await;
    runner
        .read_file(&format!("{tmp}/audio.mp3"), DEFAULT_TIMEOUT_MS)
        .await
}

pub async fn extract_captions(runner: &dyn VideoRunner, tmp: &str, url: &str) -> Option<Vec<Cue>> {
    let _ = runner.run("mkdir", &["-p", tmp], DEFAULT_TIMEOUT_MS).await;
    let _ = runner
        .run(
            "yt-dlp",
            &[
                "--no-warnings",
                "--skip-download",
                "--write-auto-sub",
                "--write-sub",
                "--sub-langs",
                "en",
                "--sub-format",
                "vtt",
                "-o",
                &format!("{tmp}/cap.%(ext)s"),
                url,
            ],
            DEFAULT_TIMEOUT_MS,
        )
        .await;
    let bytes = runner
        .read_file(&format!("{tmp}/cap.en.vtt"), DEFAULT_TIMEOUT_MS)
        .await?;
    let cues = parse_vtt(&String::from_utf8_lossy(&bytes));
    if cues.is_empty() { None } else { Some(cues) }
}

pub async fn download_clip(
    runner: &dyn VideoRunner,
    tmp: &str,
    url: &str,
    window: Window,
) -> Option<String> {
    let _ = runner.run("mkdir", &["-p", tmp], DEFAULT_TIMEOUT_MS).await;
    let _ = runner
        .run(
            "yt-dlp",
            &[
                "--no-warnings",
                "--download-sections",
                &format!("*{}-{}", window.from, window.to),
                "-f",
                "bv*[height<=480]+ba/b[height<=480]",
                "--merge-output-format",
                "mp4",
                "-o",
                &format!("{tmp}/clip.%(ext)s"),
                url,
            ],
            DEFAULT_TIMEOUT_MS,
        )
        .await;
    let found = runner
        .run("ls", &[&format!("{tmp}/clip.mp4")], DEFAULT_TIMEOUT_MS)
        .await;
    if found.ok {
        Some(format!("{tmp}/clip.mp4"))
    } else {
        None
    }
}

const FRAME_INSTRUCTION: &str = "You are shown video frames in order, oldest first. Reply with exactly one short line per frame, in the same order, describing what it shows. No numbering, no preamble, nothing before or after the lines.";

pub async fn describe_frames(
    port: &dyn ModelPort,
    frames: &[String],
) -> Result<(String, Option<ModelUsage>), String> {
    if frames.is_empty() {
        return Ok((String::new(), None));
    }
    let mut messages = utility_messages(
        FRAME_INSTRUCTION,
        format!("{} frame(s) follow, in order.", frames.len()),
    );
    let parts: Vec<ContentPart> = std::iter::once(ContentPart::Text {
        text: format!("{} frame(s) follow, in order.", frames.len()),
    })
    .chain(frames.iter().map(|url| ContentPart::ImageUrl {
        image_url: model::ImageUrl { url: url.clone() },
    }))
    .collect();
    if let Some(user) = messages.get_mut(1) {
        user.content = MessageContent::Parts(parts);
    }
    let request = ModelRequest {
        model: VISION_DEFAULT_MODEL.to_string(),
        messages,
        tools: None,
        tool_choice: None,
        reasoning: None,
        max_output_tokens: None,
    };
    let mut stream = port.stream(request);
    let mut text = String::new();
    let mut usage = None;
    while let Some(event) = stream.next().await {
        match event {
            ModelEvent::Delta { text: delta, .. } => text.push_str(&delta),
            ModelEvent::Done { usage: u, .. } => usage = u.or(usage),
            ModelEvent::ToolCalls { usage: u, .. } => usage = u.or(usage),
            ModelEvent::Error { message, .. } => return Err(message),
        }
    }
    Ok((text, usage))
}

pub fn merge_usage(acc: Option<ModelUsage>, next: Option<ModelUsage>) -> Option<ModelUsage> {
    match (acc, next) {
        (None, u) => u,
        (a, None) => a,
        (Some(c), Some(u)) => Some(ModelUsage {
            cost_usd: c.cost_usd + u.cost_usd,
            input_tokens: c.input_tokens + u.input_tokens,
            output_tokens: c.output_tokens + u.output_tokens,
            cached_tokens: c.cached_tokens + u.cached_tokens,
            cost_known: c.cost_known && u.cost_known,
        }),
    }
}

#[derive(Debug, Default)]
pub struct MediaInput {
    pub url: Option<String>,
    pub attachment_id: Option<String>,
    pub path: Option<String>,
    pub from: Option<f64>,
    pub to: Option<f64>,
}

pub struct MediaResult {
    pub ok: bool,
    pub text: String,
    pub usage: Vec<ModelUsage>,
}

fn lock_db(db: &Arc<Mutex<Db>>) -> std::sync::MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn real_recording_path(runner: &dyn VideoRunner, path: &str) -> Option<String> {
    let result = runner.run("realpath", &[path], DEFAULT_TIMEOUT_MS).await;
    if !result.ok {
        return None;
    }
    let real = result.output.trim();
    if real.is_empty() || (real != RECORDING_DIR && !real.starts_with(&format!("{RECORDING_DIR}/")))
    {
        return None;
    }
    Some(real.to_string())
}

fn attachment_host_path(data_dir: &str, id: &str) -> PathBuf {
    PathBuf::from(data_dir).join("attachments").join(id)
}

pub async fn watch_video(
    db: &Arc<Mutex<Db>>,
    data_dir: &str,
    port: &dyn ModelPort,
    runner: &dyn VideoRunner,
    resolver: &dyn Resolver,
    input: MediaInput,
) -> MediaResult {
    let url = input.url.filter(|s| !s.trim().is_empty());
    let attachment_id = input.attachment_id.filter(|s| !s.trim().is_empty());
    let raw_path = input.path.filter(|s| !s.trim().is_empty());

    if url.is_none() && attachment_id.is_none() && raw_path.is_none() {
        return MediaResult {
            ok: false,
            text: "Give either a url or an attachmentId, or a path from snap_desk/record_desk."
                .into(),
            usage: vec![],
        };
    }

    let window = bound_window(input.from, input.to);
    let mut usage: Vec<ModelUsage> = vec![];
    let tmp = tmp_dir();

    let input_path;
    let mut cues: Option<Vec<Cue>> = None;

    if let Some(url) = url {
        let allowed = match desk::may_visit(&url, resolver).await {
            Ok(u) => u,
            Err(r) => {
                return MediaResult {
                    ok: false,
                    text: r.error,
                    usage,
                };
            }
        };
        let target = allowed.to_string();
        cues = extract_captions(runner, &format!("{tmp}/cap"), &target).await;
        let Some(clip_path) = download_clip(runner, &format!("{tmp}/clip"), &target, window).await
        else {
            return MediaResult {
                ok: false,
                text: "Could not download that video.".into(),
                usage,
            };
        };
        input_path = clip_path;
    } else if let Some(raw_path) = raw_path {
        let validated = match resolve_recording_path(&raw_path) {
            Ok(p) => p,
            Err(e) => {
                return MediaResult {
                    ok: false,
                    text: e,
                    usage,
                };
            }
        };
        let Some(real) = real_recording_path(runner, &validated).await else {
            return MediaResult {
                ok: false,
                text: "That path is not a recording, or it escapes the recordings directory."
                    .into(),
                usage,
            };
        };
        if let Some(mime) = image_mime_for_path(&real) {
            let Some(bytes) = runner.read_file(&real, DEFAULT_TIMEOUT_MS).await else {
                return MediaResult {
                    ok: false,
                    text: "Could not read that recording.".into(),
                    usage,
                };
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            let data_url = format!("data:{mime};base64,{b64}");
            match describe_frames(port, &[data_url]).await {
                Ok((text, u)) => {
                    if let Some(u) = u {
                        usage.push(u);
                    }
                    let lines = build_frame_lines(&[window.from as f64], &text);
                    return MediaResult {
                        ok: true,
                        text: format!("Frames:\n{}", lines.join("\n")),
                        usage,
                    };
                }
                Err(e) => {
                    return MediaResult {
                        ok: false,
                        text: e,
                        usage,
                    };
                }
            }
        }
        input_path = real;
    } else {
        let id = attachment_id.expect("checked above");
        let attachment = {
            let guard = lock_db(db);
            get_attachment(&guard, &id).ok().flatten()
        };
        let Some(attachment) = attachment else {
            return MediaResult {
                ok: false,
                text: "No such attachment.".into(),
                usage,
            };
        };

        if is_image_content_type(&attachment.content_type) {
            let bytes = match read_attachment(data_dir, &attachment.id) {
                Ok(b) => b,
                Err(_) => {
                    return MediaResult {
                        ok: false,
                        text: "Could not read that attachment.".into(),
                        usage,
                    };
                }
            };
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            let data_url = format!("data:{};base64,{}", attachment.content_type, b64);
            match describe_frames(port, &[data_url]).await {
                Ok((text, u)) => {
                    if let Some(u) = u {
                        usage.push(u);
                    }
                    let lines = build_frame_lines(&[window.from as f64], &text);
                    return MediaResult {
                        ok: true,
                        text: format!("Frames:\n{}", lines.join("\n")),
                        usage,
                    };
                }
                Err(e) => {
                    return MediaResult {
                        ok: false,
                        text: e,
                        usage,
                    };
                }
            }
        }

        let _ = runner.run("mkdir", &["-p", &tmp], DEFAULT_TIMEOUT_MS).await;
        let dest = format!("{tmp}/input");
        let host = attachment_host_path(data_dir, &attachment.id);
        let host_str = host.to_string_lossy();
        if !runner.copy_in(&host_str, &dest).await {
            return MediaResult {
                ok: false,
                text: "Could not get that file onto the desk.".into(),
                usage,
            };
        }
        input_path = dest;
    }

    let (frames, times) =
        extract_frames(runner, &format!("{tmp}/frames"), &input_path, window).await;
    let mut frame_lines = Vec::new();
    if !frames.is_empty() {
        match describe_frames(port, &frames).await {
            Ok((text, u)) => {
                if let Some(u) = u {
                    usage.push(u);
                }
                frame_lines = build_frame_lines(&times, &text);
            }
            Err(e) => {
                return MediaResult {
                    ok: false,
                    text: e,
                    usage,
                };
            }
        }
    }

    let mut transcript_text = String::new();
    if let Some(ref cue_list) = cues {
        transcript_text = transcript_from_cues(cue_list, window);
    } else if let Some(audio) = extract_audio(runner, &format!("{tmp}/audio"), &input_path, window)
        .await
        .filter(|audio| !audio.is_empty())
    {
        let result = transcribe::transcribe(
            &audio,
            "audio/mpeg",
            TranscribeOptions {
                model: None,
                api_key: None,
                timeout_ms: DEFAULT_TIMEOUT_MS,
                http: None,
            },
        )
        .await;
        if let Some(u) = result.usage {
            usage.push(u);
        }
        transcript_text = if result.ok {
            result.text
        } else {
            format!("(transcription failed: {})", result.detail)
        };
    }

    let mut parts = Vec::new();
    if !transcript_text.is_empty() {
        parts.push(format!("Transcript:\n{transcript_text}"));
    }
    if !frame_lines.is_empty() {
        parts.push(format!("Frames:\n{}", frame_lines.join("\n")));
    }
    if parts.is_empty() {
        return MediaResult {
            ok: false,
            text: "Nothing could be extracted from that.".into(),
            usage,
        };
    }

    MediaResult {
        ok: true,
        text: parts.join("\n\n"),
        usage,
    }
}
