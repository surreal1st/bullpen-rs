//! S12-06: link previews — port of `link-preview.test.ts`.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use server::desk::RealResolver;
use server::link_preview::{
    PreviewOptions, image_allowed, links_in, preview_for, twitch_login, youtube_id,
};
use server::web_fetch::{FetchedHttp, WebFetch};
use url::Url;

const TWITCH_LIVE: &str = r#"{
  "data": {
    "user": {
      "displayName": "yaya_live_",
      "profileImageURL": "https://static-cdn.jtvnw.net/jtv_user_pictures/0dda0449-profile_image-300x300.png",
      "description": "Je joue (beaucoup) aux jeux vidéo en ligne",
      "stream": {
        "title": "[FR/ENG] <Advance> Ula'tek Mythic 7/8MM PREVOKER POV",
        "type": "live",
        "viewersCount": 16,
        "previewImageURL": "https://static-cdn.jtvnw.net/previews-ttv/live_user_yaya_live_-640x360.jpg",
        "game": { "name": "World of Warcraft" }
      }
    }
  }
}"#;

const YT_OEMBED: &str = r#"{
  "title": "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)",
  "author_name": "Rick Astley",
  "thumbnail_url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"
}"#;

struct StubFetch {
    responses: Mutex<HashMap<String, FetchedHttp>>,
    calls: Mutex<Vec<String>>,
}

impl StubFetch {
    fn json(body: &str) -> FetchedHttp {
        FetchedHttp {
            status: 200,
            content_type: "application/json".to_string(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn with(self, url: &str, http: FetchedHttp) -> Self {
        self.responses.lock().unwrap().insert(url.to_string(), http);
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Default for StubFetch {
    fn default() -> Self {
        Self {
            responses: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl WebFetch for StubFetch {
    async fn get(&self, url: &str, _timeout_ms: u64) -> Result<FetchedHttp, String> {
        self.calls.lock().unwrap().push(url.to_string());
        self.responses
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| format!("no stub for {url}"))
    }

    async fn post_json(
        &self,
        url: &str,
        _headers: &[(&str, &str)],
        _body: &str,
        _timeout_ms: u64,
    ) -> Result<FetchedHttp, String> {
        self.calls.lock().unwrap().push(url.to_string());
        self.responses
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| format!("no stub for {url}"))
    }
}

fn memory_db() -> Arc<Mutex<store::Db>> {
    Arc::new(Mutex::new(store::Db::open(":memory:").expect("open")))
}

#[test]
fn youtube_id_shapes() {
    let shapes = [
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://youtu.be/dQw4w9WgXcQ",
        "https://www.youtube.com/shorts/dQw4w9WgXcQ",
        "https://m.youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
    ];
    for shape in shapes {
        assert_eq!(
            youtube_id(&Url::parse(shape).unwrap()).as_deref(),
            Some("dQw4w9WgXcQ"),
            "{shape}"
        );
    }
    assert!(youtube_id(&Url::parse("https://example.com/watch?v=x").unwrap()).is_none());
}

#[test]
fn twitch_login_channels_only() {
    assert_eq!(
        twitch_login(&Url::parse("https://twitch.tv/yaya_live_").unwrap()).as_deref(),
        Some("yaya_live_")
    );
    assert_eq!(
        twitch_login(&Url::parse("https://www.twitch.tv/YaYa_Live_").unwrap()).as_deref(),
        Some("yaya_live_")
    );
    assert!(twitch_login(&Url::parse("https://twitch.tv/directory/game/WoW").unwrap()).is_none());
    assert!(twitch_login(&Url::parse("https://twitch.tv/videos/123").unwrap()).is_none());
}

#[test]
fn links_in_trims_punctuation() {
    let text =
        "live now: https://twitch.tv/yaya_live_. Also (https://youtu.be/abc123) worth a look.";
    let found = links_in(text, 3);
    assert!(found.contains(&"https://twitch.tv/yaya_live_".to_string()));
    assert!(found.contains(&"https://youtu.be/abc123".to_string()));
}

#[test]
fn links_in_caps_count() {
    let text = (0..9)
        .map(|i| format!("https://twitch.tv/chan{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(links_in(&text, 3).len(), 3);
}

#[test]
fn image_allowed_https_only() {
    assert!(image_allowed("https://i.ytimg.com/vi/x/hqdefault.jpg"));
    assert!(!image_allowed("http://i.ytimg.com/vi/x/hqdefault.jpg"));
    assert!(!image_allowed("file:///etc/passwd"));
    assert!(!image_allowed("javascript:alert(1)"));
    assert!(!image_allowed("not a url"));
}

#[tokio::test]
async fn preview_twitch_live() {
    let db = memory_db();
    let fetch =
        StubFetch::default().with("https://gql.twitch.tv/gql", StubFetch::json(TWITCH_LIVE));
    let preview = preview_for(
        &db,
        "https://twitch.tv/yaya_live_",
        PreviewOptions {
            fetch: &fetch,
            resolver: &RealResolver,
            now: None,
        },
    )
    .await
    .expect("preview");
    assert_eq!(preview.provider, "Twitch");
    assert_eq!(preview.live, Some(true));
    assert!(preview.title.contains("PREVOKER POV"));
    assert!(preview.subtitle.contains("World of Warcraft"));
    assert!(preview.subtitle.contains("16 watching"));
    assert!(preview.image.contains("previews-ttv"));
}

#[tokio::test]
async fn preview_twitch_offline_profile_image() {
    let db = memory_db();
    let offline = r#"{
      "data": { "user": {
        "displayName": "yaya_live_",
        "profileImageURL": "https://static-cdn.jtvnw.net/jtv_user_pictures/profile_image.png",
        "description": "offline",
        "stream": null
      }}
    }"#;
    let fetch = StubFetch::default().with("https://gql.twitch.tv/gql", StubFetch::json(offline));
    let preview = preview_for(
        &db,
        "https://twitch.tv/yaya_live_",
        PreviewOptions {
            fetch: &fetch,
            resolver: &RealResolver,
            now: None,
        },
    )
    .await
    .expect("preview");
    assert_eq!(preview.live, Some(false));
    assert!(preview.image.contains("profile_image"));
}

#[tokio::test]
async fn preview_youtube_oembed() {
    let db = memory_db();
    let fetch = StubFetch::default().with(
        "https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DdQw4w9WgXcQ&format=json",
        StubFetch::json(YT_OEMBED),
    );
    let preview = preview_for(
        &db,
        "https://youtu.be/dQw4w9WgXcQ",
        PreviewOptions {
            fetch: &fetch,
            resolver: &RealResolver,
            now: None,
        },
    )
    .await
    .expect("preview");
    assert_eq!(preview.provider, "YouTube");
    assert!(preview.title.contains("Never Gonna Give You Up"));
    assert_eq!(preview.subtitle, "Rick Astley");
    assert!(fetch.calls()[0].contains("oembed"));
}

#[tokio::test]
async fn cached_preview_hits_once() {
    let db = memory_db();
    let fetch = StubFetch::default().with(
        "https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DdQw4w9WgXcQ&format=json",
        StubFetch::json(YT_OEMBED),
    );
    let preview_opts = || PreviewOptions {
        fetch: &fetch,
        resolver: &RealResolver,
        now: None,
    };
    preview_for(&db, "https://youtu.be/dQw4w9WgXcQ", preview_opts()).await;
    preview_for(&db, "https://youtu.be/dQw4w9WgXcQ", preview_opts()).await;
    assert_eq!(fetch.calls().len(), 1);
}

#[tokio::test]
async fn twitch_ttl_refreshes_before_youtube() {
    let db = memory_db();
    let twitch =
        StubFetch::default().with("https://gql.twitch.tv/gql", StubFetch::json(TWITCH_LIVE));
    let yt = StubFetch::default().with(
        "https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DdQw4w9WgXcQ&format=json",
        StubFetch::json(YT_OEMBED),
    );
    let start = chrono::DateTime::parse_from_rfc3339("2026-09-11T18:00:00Z")
        .unwrap()
        .timestamp_millis();
    let later = start + 20 * 60 * 1000;

    preview_for(
        &db,
        "https://twitch.tv/yaya_live_",
        PreviewOptions {
            fetch: &twitch,
            resolver: &RealResolver,
            now: Some(start),
        },
    )
    .await;
    preview_for(
        &db,
        "https://twitch.tv/yaya_live_",
        PreviewOptions {
            fetch: &twitch,
            resolver: &RealResolver,
            now: Some(later),
        },
    )
    .await;
    assert_eq!(twitch.calls().len(), 2);

    preview_for(
        &db,
        "https://youtu.be/dQw4w9WgXcQ",
        PreviewOptions {
            fetch: &yt,
            resolver: &RealResolver,
            now: Some(start),
        },
    )
    .await;
    preview_for(
        &db,
        "https://youtu.be/dQw4w9WgXcQ",
        PreviewOptions {
            fetch: &yt,
            resolver: &RealResolver,
            now: Some(later),
        },
    )
    .await;
    assert_eq!(yt.calls().len(), 1);
}

struct FailFetch;

#[async_trait]
impl WebFetch for FailFetch {
    async fn get(&self, _url: &str, _timeout_ms: u64) -> Result<FetchedHttp, String> {
        Err("network gone".to_string())
    }
}

#[tokio::test]
async fn preview_errors_return_none() {
    let db = memory_db();
    let preview = preview_for(
        &db,
        "https://youtu.be/dQw4w9WgXcQ",
        PreviewOptions {
            fetch: &FailFetch,
            resolver: &RealResolver,
            now: None,
        },
    )
    .await;
    assert!(preview.is_none());
}

#[tokio::test]
async fn preview_refuses_non_http_scheme() {
    let db = memory_db();
    let fetch = StubFetch::default().with(
        "https://www.youtube.com/oembed?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DdQw4w9WgXcQ&format=json",
        StubFetch::json(YT_OEMBED),
    );
    let preview = preview_for(
        &db,
        "file:///etc/passwd",
        PreviewOptions {
            fetch: &fetch,
            resolver: &RealResolver,
            now: None,
        },
    )
    .await;
    assert!(preview.is_none());
    assert!(fetch.calls().is_empty());
}
