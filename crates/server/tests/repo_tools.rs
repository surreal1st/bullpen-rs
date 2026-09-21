//! S9-07: repo tools through the real toolbox + HTTP repo routes.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use model::ladder::Trigger;
use model::routing::set_routing_settings;
use server::build_app;
use server::repo::{self, RepoConfig};
use server::runs::RunManager;
use server::sandbox::{ExecResult, Sandbox};
use store::Db;
use store::bots::{BotDraft, create_bot};
use tower::ServiceExt;

#[allow(dead_code)]
struct FakeRepoSandbox {
    files: Mutex<HashMap<String, String>>,
    cloned: Mutex<bool>,
    branch: Mutex<String>,
    known_branches: Mutex<std::collections::HashSet<String>>,
    commands: Mutex<Vec<String>>,
    run_responses: Mutex<HashMap<String, ExecResult>>,
}

#[allow(dead_code)]
impl FakeRepoSandbox {
    fn new(initial: HashMap<String, String>) -> Self {
        let mut files = HashMap::new();
        for (k, v) in initial {
            files.insert(format!("/work/repo/{k}"), v);
        }
        let mut branches = std::collections::HashSet::new();
        branches.insert("main".to_string());
        Self {
            files: Mutex::new(files),
            cloned: Mutex::new(true),
            branch: Mutex::new("main".to_string()),
            known_branches: Mutex::new(branches),
            commands: Mutex::new(Vec::new()),
            run_responses: Mutex::new(HashMap::new()),
        }
    }

    fn set_cloned(&self, v: bool) {
        *self.cloned.lock().unwrap() = v;
    }

    fn on_run(&self, command: &str, result: ExecResult) {
        self.run_responses
            .lock()
            .unwrap()
            .insert(command.to_string(), result);
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

fn ok(stdout: impl Into<String>) -> ExecResult {
    ExecResult {
        stdout: stdout.into(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
        truncated: false,
        unavailable: false,
    }
}

#[async_trait]
impl Sandbox for FakeRepoSandbox {
    async fn exec(&self, _bot_id: &str, command: &str) -> ExecResult {
        self.commands.lock().unwrap().push(command.to_string());
        if command.contains("test -d /work/repo/.git") {
            let cloned = *self.cloned.lock().unwrap();
            return ok(if cloned {
                "yes\n".to_string()
            } else {
                "no\n".to_string()
            });
        }
        if command.starts_with("git clone") {
            *self.cloned.lock().unwrap() = true;
            return ok("Cloning into '/work/repo'...\n");
        }
        if let Some(cap) = regex::Regex::new(r"printf '%s' '([^']*)' \| base64 -d > '([^']+)'")
            .unwrap()
            .captures(command)
        {
            let b64 = cap.get(1).unwrap().as_str();
            let path = cap.get(2).unwrap().as_str();
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                .unwrap_or_default();
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), String::from_utf8_lossy(&bytes).into());
            return ok(String::new());
        }
        if let Some(cap) = regex::Regex::new(r"^base64 '([^']+)'")
            .unwrap()
            .captures(command)
        {
            let path = cap.get(1).unwrap().as_str();
            let content = self.files.lock().unwrap().get(path).cloned();
            return match content {
                Some(c) => ok(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    c.as_bytes(),
                )),
                None => ExecResult {
                    stdout: format!("base64: can't open '{path}': No such file"),
                    stderr: String::new(),
                    exit_code: 1,
                    timed_out: false,
                    truncated: false,
                    unavailable: false,
                },
            };
        }
        if command.contains("grep -rn") {
            let pattern = regex::Regex::new(r"-- '([^']*)'")
                .unwrap()
                .captures(command)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let mut out = Vec::new();
            for (path, content) in self.files.lock().unwrap().iter() {
                for (i, line) in content.split('\n').enumerate() {
                    if !pattern.is_empty() && line.contains(&pattern) {
                        out.push(format!(
                            "{}:{}:{}",
                            path.replace("/work/repo/", "./"),
                            i + 1,
                            line
                        ));
                    }
                }
            }
            return ok(out.join("\n"));
        }
        if command.contains("git rev-parse --verify") {
            let name = regex::Regex::new(r"--verify '([^']*)'")
                .unwrap()
                .captures(command)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let exists = self.known_branches.lock().unwrap().contains(&name);
            return ok(if exists {
                "exists\n".to_string()
            } else {
                "new\n".to_string()
            });
        }
        if command.contains("git checkout") {
            let name = regex::Regex::new(r"checkout(?: -b)? '([^']+)'")
                .unwrap()
                .captures(command)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string());
            if let Some(b) = name {
                *self.branch.lock().unwrap() = b.clone();
                self.known_branches.lock().unwrap().insert(b);
            }
            return ok(String::new());
        }
        if let Some(cap) = regex::Regex::new(r"^cd /work/repo && \{ ([\s\S]*) ; \}$")
            .unwrap()
            .captures(command)
        {
            let inner = cap.get(1).unwrap().as_str();
            return self
                .run_responses
                .lock()
                .unwrap()
                .get(inner)
                .cloned()
                .unwrap_or_else(|| ok(String::new()));
        }
        ok(String::new())
    }

    async fn read_file(&self, _bot_id: &str, _path: &str) -> Result<String, String> {
        Err("not used".into())
    }
}

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open db");
    repo::ensure_bot_repo_column(&db).expect("repo column");
    set_routing_settings(&db, Some(false), None).expect("routing off");
    Arc::new(Mutex::new(db))
}

fn seed_bot_with_repo(db: &Db, repo: &RepoConfig) -> String {
    let bot = create_bot(
        db,
        BotDraft {
            name: "RepoBot".into(),
            purpose: "p".into(),
            instructions: "test".into(),
            model: None,
        },
    )
    .expect("bot");
    let json = serde_json::to_value(repo).unwrap();
    repo::set_bot_repo(db, &bot.id, &json).expect("set repo");
    store::set_bot_egress(
        db,
        &bot.id,
        r#"{"mode":"allowlist","allow":["github.com"]}"#,
    )
    .expect("egress");
    bot.id
}

#[tokio::test]
async fn repo_specs_offered_only_when_bot_has_repo() {
    let db = open_db();
    let bot_no_repo = {
        let g = db.lock().unwrap();
        create_bot(
            &g,
            BotDraft {
                name: "NoRepo".into(),
                purpose: "p".into(),
                instructions: "x".into(),
                model: None,
            },
        )
        .unwrap()
        .id
    };
    let bot_with_repo = {
        let g = db.lock().unwrap();
        seed_bot_with_repo(
            &g,
            &RepoConfig {
                url: "https://github.com/a/b".into(),
                branch: "main".into(),
                setup_command: None,
            },
        )
    };
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeRepoSandbox::new(HashMap::new()));
    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        Arc::new(model::fake::FakePort::new(vec![])),
        sandbox,
    ));
    let toolbox_no_repo =
        manager.toolbox_for(&bot_no_repo, Trigger::Chat, false, "test/model", None);
    let empty: std::collections::HashSet<_> = toolbox_no_repo
        .specs
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(!empty.contains("repo_read"));
    let toolbox_with_repo =
        manager.toolbox_for(&bot_with_repo, Trigger::Chat, false, "test/model", None);
    let with: std::collections::HashSet<_> = toolbox_with_repo
        .specs
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    for name in ["repo_read", "repo_grep", "repo_run", "repo_branch"] {
        assert!(with.contains(name), "missing {name}");
    }
    assert!(!with.contains("repo_edit"));
    assert!(!with.contains("repo_pr"));
}

#[tokio::test]
async fn repo_read_and_grep_through_toolbox() {
    unsafe { std::env::set_var("BULLPEN_SANDBOX_EGRESS", "on") };
    let db = open_db();
    let bot = {
        let g = db.lock().unwrap();
        seed_bot_with_repo(
            &g,
            &RepoConfig {
                url: "https://github.com/a/b".into(),
                branch: "main".into(),
                setup_command: None,
            },
        )
    };
    let mut files = HashMap::new();
    files.insert("README.md".into(), "hello world\n".into());
    files.insert("src/lib.rs".into(), "fn main() {}\n".into());
    let sandbox: Arc<dyn Sandbox> = Arc::new(FakeRepoSandbox::new(files));
    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        Arc::new(model::fake::FakePort::new(vec![])),
        Arc::clone(&sandbox),
    ));
    let toolbox = manager.toolbox_for(&bot, Trigger::Chat, false, "test/model", None);
    let read = toolbox
        .run("repo_read", r#"{"path":"README.md"}"#)
        .await
        .text;
    assert!(read.contains("hello world"));
    let grep = toolbox
        .run("repo_grep", r#"{"pattern":"fn main"}"#)
        .await
        .text;
    assert!(grep.contains("lib.rs"));
}

#[tokio::test]
async fn repo_http_get_put() {
    let db = Db::open(":memory:").expect("db");
    repo::ensure_bot_repo_column(&db).expect("column");
    let cookie = seed_session(&db);
    let bot = create_bot(
        &db,
        BotDraft {
            name: "HttpBot".into(),
            purpose: "p".into(),
            instructions: "x".into(),
            model: None,
        },
    )
    .unwrap();
    let app = build_app(server::AppState::new(db));
    let put_body = serde_json::json!({
        "repo": {
            "url": "https://github.com/rainmade/bullpen-rs",
            "branch": "main"
        }
    });
    let put = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/bots/{}/repo", bot.id))
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(Body::from(put_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);
    let get = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/bots/{}/repo", bot.id))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::OK);
}

#[tokio::test]
async fn prepare_repo_refuses_before_sandbox_when_egress_off() {
    let fake = FakeRepoSandbox::new(HashMap::new());
    let err = repo::prepare_repo(
        &fake,
        "bot",
        &RepoConfig {
            url: "https://github.com/a/b".into(),
            branch: "main".into(),
            setup_command: None,
        },
        &server::egress::DEFAULT_BOT_EGRESS,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("BULLPEN_SANDBOX_EGRESS"));
    assert!(fake.commands().is_empty());
}
