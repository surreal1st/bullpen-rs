//! S12-05: port of `test/video.test.ts` and `test/review-media-path.test.ts`.

use async_trait::async_trait;
use base64::Engine;
use model::fake::FakePort;
use model::{ModelEvent, ModelUsage};
use server::permissions::{Decision, default_decisions, tighten_set};
use server::video::{
    MAX_VIDEO_SECONDS, MediaInput, RECORDING_DIR, RunResult, VideoRunner, bound_window,
    build_frame_lines, describe_frames, fmt_time, merge_usage, parse_showinfo_times, parse_vtt,
    resolve_recording_path, transcript_from_cues, watch_video,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use store::Db;

struct FakeVideoRunner {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
}

impl FakeVideoRunner {
    fn new(files: HashMap<String, Vec<u8>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            files: Mutex::new(files),
        }
    }
}

#[async_trait]
impl VideoRunner for FakeVideoRunner {
    async fn run(&self, cmd: &str, args: &[&str], _timeout_ms: u64) -> RunResult {
        self.calls.lock().unwrap().push((
            cmd.to_string(),
            args.iter().map(|s| s.to_string()).collect(),
        ));
        if cmd == "realpath" {
            return RunResult {
                ok: true,
                output: args.first().copied().unwrap_or("").to_string(),
            };
        }
        RunResult {
            ok: true,
            output: String::new(),
        }
    }

    async fn read_file(&self, path: &str, _timeout_ms: u64) -> Option<Vec<u8>> {
        self.files.lock().unwrap().get(path).cloned()
    }

    async fn copy_in(&self, _host: &str, _dest: &str) -> bool {
        true
    }
}

fn fake_vision_port(reply: &str) -> FakePort {
    FakePort::new(vec![
        ModelEvent::Delta {
            text: reply.to_string(),
        },
        ModelEvent::Done {
            model: "test/vision".into(),
            usage: Some(ModelUsage {
                cost_usd: 0.002,
                input_tokens: 500,
                output_tokens: 20,
                cached_tokens: 0,
                cost_known: true,
            }),
            finish_reason: None,
        },
    ])
}

#[test]
fn bound_window_defaults_and_caps() {
    let w = bound_window(None, None);
    assert_eq!(w.from, 0);
    assert_eq!(w.to, MAX_VIDEO_SECONDS);
    assert_eq!(bound_window(Some(10.0), Some(40.0)).to, 40);
    let long = bound_window(Some(0.0), Some((MAX_VIDEO_SECONDS * 10) as f64));
    assert_eq!(long.to - long.from, MAX_VIDEO_SECONDS);
}

#[test]
fn fmt_time_and_showinfo() {
    assert_eq!(fmt_time(65), "1:05");
    assert_eq!(fmt_time(3725), "1:02:05");
    let out = "[Parsed_showinfo_1] n:0 pts_time:0.5\n[Parsed_showinfo_1] n:1 pts_time:12.75";
    assert_eq!(parse_showinfo_times(out), vec![0.5, 12.75]);
}

#[test]
fn parse_vtt_and_transcript_dedupes() {
    let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:03.500\nHello <b>there</b>\n\n00:00:04.000 --> 00:00:06.000 align:start\nSecond line";
    let cues = parse_vtt(vtt);
    assert_eq!(cues.len(), 2);
    assert_eq!(cues[0].text, "Hello there");
    let dup = "00:00:01.000 --> 00:00:02.000\nthe quick\n\n00:00:02.000 --> 00:00:03.000\nthe quick\n\n00:00:03.000 --> 00:00:04.000\nbrown fox";
    let text = transcript_from_cues(&parse_vtt(dup), server::video::Window { from: 0, to: 10 });
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        vec!["[0:01] the quick", "[0:03] brown fox"]
    );
}

#[test]
fn build_frame_lines_pads_and_strips_numbering() {
    assert_eq!(
        build_frame_lines(&[1.0, 2.0, 3.0], "a\nb"),
        vec!["[0:01] a", "[0:02] b", "[0:03] (no description)"]
    );
    assert_eq!(
        build_frame_lines(&[5.0], "1. a red screen"),
        vec!["[0:05] a red screen"]
    );
}

#[tokio::test]
async fn describe_frames_batches_images() {
    let port = fake_vision_port("first\nsecond");
    let frames = vec![
        "data:image/png;base64,AAA".into(),
        "data:image/png;base64,BBB".into(),
    ];
    let (text, usage) = describe_frames(&port, &frames).await.unwrap();
    assert_eq!(text, "first\nsecond");
    assert!(usage.is_some());
    let requests = port.requests();
    assert_eq!(requests.len(), 1);
    match &requests[0].messages[1].content {
        model::MessageContent::Parts(parts) => {
            assert_eq!(parts.len(), 3);
        }
        _ => panic!("expected parts"),
    }
}

#[test]
fn resolve_recording_path_narrower_than_clip() {
    assert!(resolve_recording_path("/work/recording.mp4").is_err());
    assert!(resolve_recording_path(&format!("{RECORDING_DIR}/desk.mp4")).is_ok());
    assert!(resolve_recording_path("../etc/passwd").is_err());
}

#[tokio::test]
async fn watch_video_refuses_missing_source() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    let runner = FakeVideoRunner::new(HashMap::new());
    let port = fake_vision_port("n/a");
    let result = watch_video(
        &db,
        "unused",
        &port,
        &runner,
        &server::desk::RealResolver,
        MediaInput::default(),
    )
    .await;
    assert!(!result.ok);
    assert!(result.text.contains("url or an attachmentId"));
}

#[tokio::test]
async fn review_media_png_skips_ffmpeg() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    let dir = tempfile::tempdir().unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let path = format!("{RECORDING_DIR}/desk-shot.png");
    let runner = FakeVideoRunner::new(HashMap::from([(path.clone(), png)]));
    let port = fake_vision_port("a desktop with a browser window open");
    let result = watch_video(
        &db,
        dir.path().to_str().unwrap(),
        &port,
        &runner,
        &server::desk::RealResolver,
        MediaInput {
            path: Some(path),
            ..Default::default()
        },
    )
    .await;
    assert!(result.ok);
    assert!(result.text.contains("browser window"));
    let calls = runner.calls.lock().unwrap();
    assert!(
        !calls
            .iter()
            .any(|(cmd, _)| cmd == "ffmpeg" || cmd == "yt-dlp")
    );
}

#[test]
fn permissions_ask_and_tighten() {
    let d = default_decisions();
    assert_eq!(d.get("watch_video"), Some(&Decision::Ask));
    assert_eq!(d.get("review_media"), Some(&Decision::Ask));
    let t = tighten_set();
    assert!(t.contains(&"watch_video"));
    assert!(t.contains(&"review_media"));
}

#[test]
fn merge_usage_sums_cost() {
    let a = ModelUsage {
        cost_usd: 0.1,
        input_tokens: 1,
        output_tokens: 2,
        cached_tokens: 0,
        cost_known: true,
    };
    let b = ModelUsage {
        cost_usd: 0.2,
        input_tokens: 3,
        output_tokens: 4,
        cached_tokens: 0,
        cost_known: true,
    };
    let m = merge_usage(Some(a), Some(b)).unwrap();
    assert!((m.cost_usd - 0.3).abs() < f64::EPSILON);
}
