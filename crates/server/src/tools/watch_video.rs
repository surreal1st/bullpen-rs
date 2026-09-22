//! S12-05: `watch_video` and `review_media` tools.

use std::sync::{Arc, Mutex};

use model::{ModelPort, ModelUsage, ToolSpec};
use store::Db;

use crate::desk::{self, RealResolver};
use crate::observations::{DesktopStateRegistry, ObservationRegistry};
use crate::video::{
    DockerVideoRunner, MAX_FRAMES, MAX_VIDEO_SECONDS, MediaInput, merge_usage, watch_video,
};
use crate::vm;

pub fn watch_video_spec() -> ToolSpec {
    ToolSpec {
        name: "watch_video".to_string(),
        description: format!(
            "Watch a video and get back a transcript with timestamps plus a description of each scene-change frame, up to {MAX_FRAMES} frames. Give either a url or an attachmentId, never both. Runs in your desk container - it can take a minute for a long clip. Bound from/to (seconds) to watch only part of a long video; the window is capped at {} minutes.",
            MAX_VIDEO_SECONDS / 60
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "A video page or direct video URL." },
                "attachmentId": { "type": "string", "description": "An attachment already in this chat, instead of a url." },
                "from": { "type": "number", "description": "Start second. Default 0." },
                "to": { "type": "number", "description": "End second. Default: capped window from from." }
            }
        }),
    }
}

pub fn review_media_spec() -> ToolSpec {
    ToolSpec {
        name: "review_media".to_string(),
        description: "Look closely at one image or short video clip and describe it - the same watch_video path, narrowed to one source. Give either an attachmentId already in this chat, or a path returned by snap_desk/record_desk (must be inside the desk's recordings directory), never both.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "attachmentId": { "type": "string", "description": "The attachment's id." },
                "path": { "type": "string", "description": "A path snap_desk or record_desk gave back, instead of an attachmentId." },
                "from": { "type": "number", "description": "Start second, for a clip. Default 0." },
                "to": { "type": "number", "description": "End second, for a clip." }
            }
        }),
    }
}

pub struct VideoRunEnv {
    pub vm_docker: Arc<dyn vm::DockerRun>,
    pub vm_config: Arc<store::vms::VmConfig>,
    pub vm_enabled: bool,
    pub desktop_states: Arc<DesktopStateRegistry>,
    pub observations: Arc<ObservationRegistry>,
}

fn parse_input(args: &str, allow_url: bool) -> MediaInput {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::json!({}));
    MediaInput {
        url: if allow_url {
            v.get("url").and_then(|x| x.as_str()).map(str::to_string)
        } else {
            None
        },
        attachment_id: v
            .get("attachmentId")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        path: v.get("path").and_then(|x| x.as_str()).map(str::to_string),
        from: v.get("from").and_then(|x| x.as_f64()),
        to: v.get("to").and_then(|x| x.as_f64()),
    }
}

pub async fn run_watch_video(
    db: &Arc<Mutex<Db>>,
    data_dir: &str,
    port: &Arc<dyn ModelPort>,
    bot_id: &str,
    args: &str,
    env: &VideoRunEnv,
) -> (String, Option<ModelUsage>) {
    run_video(db, data_dir, port, bot_id, args, env, true).await
}

pub async fn run_review_media(
    db: &Arc<Mutex<Db>>,
    data_dir: &str,
    port: &Arc<dyn ModelPort>,
    bot_id: &str,
    args: &str,
    env: &VideoRunEnv,
) -> (String, Option<ModelUsage>) {
    run_video(db, data_dir, port, bot_id, args, env, false).await
}

async fn run_video(
    db: &Arc<Mutex<Db>>,
    data_dir: &str,
    port: &Arc<dyn ModelPort>,
    bot_id: &str,
    args: &str,
    env: &VideoRunEnv,
    allow_url: bool,
) -> (String, Option<ModelUsage>) {
    if !env.vm_enabled {
        return (
            "Watching video is not available when the desk is off.".into(),
            None,
        );
    }
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
            let runner = DockerVideoRunner::new(Arc::clone(&env.vm_docker), &cfg);
            let input = parse_input(args, allow_url);
            let resolver = RealResolver;
            let result = watch_video(db, data_dir, port.as_ref(), &runner, &resolver, input).await;
            drop(guard);
            let mut merged = None;
            for u in result.usage {
                merged = merge_usage(merged, Some(u));
            }
            (result.text, merged)
        }
        Err(e) => (e, None),
    }
}
