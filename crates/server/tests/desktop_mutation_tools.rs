mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{own_conversation, seed_bot};
use futures::{SinkExt, StreamExt};
use model::ladder::Trigger;
use model::{EventStream, ModelPort, ModelRequest};
use server::runs::RunManager;
use server::sandbox;
use server::vm::{CapturedFrame, DockerRun};
use store::Db;
use store::vms::{DockerResult, VmConfig};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

struct NeverCalledPort;

impl ModelPort for NeverCalledPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        panic!("direct desktop tool test must not reach model")
    }
}

struct RunningDocker;

#[async_trait]
impl DockerRun for RunningDocker {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        assert_eq!(args.first(), Some(&"inspect"));
        DockerResult {
            ok: true,
            stdout: "running true".into(),
            stderr: String::new(),
        }
    }
}

fn config() -> VmConfig {
    VmConfig {
        image: "test".into(),
        docker_host: "unix:///test.sock".into(),
        cdp_base: 9500,
        web_base: 6500,
        slots: 8,
        idle_ms: 1_800_000,
        init_dir: "/vm-init".into(),
        memory: "3g".into(),
        cpus: "1.5".into(),
        shm_size: "1g".into(),
        timezone: "UTC".into(),
        puid: "1004".into(),
        pgid: "1004".into(),
    }
}

fn store_observation(manager: &RunManager, run_id: &str, generation: u64) {
    let observation = manager
        .observation_admission()
        .try_begin_capture()
        .expect("observation admission")
        .retain(
            CapturedFrame {
                png: vec![1, 2, 3],
                width: 100,
                height: 100,
            },
            run_id,
            "arthur",
            format!("obs-{run_id}"),
            "2026-09-18T12:00:00Z",
            generation,
        );
    manager.observation_registry().store(&observation);
}

async fn spawn_cdp(
    observations: Arc<server::observations::ObservationRegistry>,
    mutation_saw_observation: Arc<AtomicBool>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let observations = Arc::clone(&observations);
            let mutation_saw_observation = Arc::clone(&mutation_saw_observation);
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = stream.peek(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                if head.to_ascii_lowercase().contains("upgrade: websocket") {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    let Some(Ok(Message::Text(text))) = ws.next().await else {
                        return;
                    };
                    let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let id = request["id"].as_u64().unwrap();
                    let method = request["method"].as_str().unwrap();
                    let expression = request["params"]["expression"].as_str().unwrap_or("");
                    let mutating = method == "Page.navigate"
                        || expression.contains("hit.click()")
                        || expression.contains("el.value =");
                    if mutating && !observations.is_empty() {
                        mutation_saw_observation.store(true, Ordering::SeqCst);
                    }
                    let value = if expression == "location.href" {
                        serde_json::json!("http://93.184.216.34/")
                    } else if expression == "document.title" {
                        serde_json::json!("Example")
                    } else if expression.contains("hit.click()") {
                        serde_json::json!("clicked: Go")
                    } else if expression.contains("el.value =") {
                        serde_json::json!("typed into q")
                    } else {
                        serde_json::json!("Body")
                    };
                    let result = if method == "Runtime.evaluate" {
                        serde_json::json!({"result": {"value": value}})
                    } else {
                        serde_json::json!({})
                    };
                    let reply = serde_json::json!({"id": id, "result": result}).to_string();
                    let _ = ws.send(Message::text(reply)).await;
                    return;
                }
                let body = if head.contains("/json/list") {
                    serde_json::json!([{"id": "target-1"}]).to_string()
                } else {
                    "{}".to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    port
}

#[tokio::test]
async fn toolbox_browser_mutations_invalidate_before_cdp_while_read_page_preserves() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    seed_bot(&db, "arthur", "Arthur");
    let _conversation = own_conversation(&db, "arthur");
    server::desk::ensure_desk_tables(&db.lock().unwrap()).unwrap();

    let manager = Arc::new(RunManager::with_sandbox_and_vm(
        Arc::clone(&db),
        Arc::new(NeverCalledPort),
        sandbox::default_sandbox(),
        Arc::new(RunningDocker),
        Arc::new(config()),
        true,
    ));
    let mutation_saw_observation = Arc::new(AtomicBool::new(false));
    let port = spawn_cdp(
        manager.observation_registry(),
        Arc::clone(&mutation_saw_observation),
    )
    .await;
    {
        let db = db.lock().unwrap();
        db.conn()
            .execute(
                "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
                 VALUES ('arthur', 'bullpen-vm-arthur', ?1, 6501, 'running', '2026-09-18T12:00:00Z')",
                [i64::from(port)],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO desk_windows (bot_id, target_id, opened_at)
                 VALUES ('arthur', 'target-1', '2026-09-18T12:00:00Z')",
                [],
            )
            .unwrap();
    }
    let toolbox = manager.toolbox_for("arthur", Trigger::Chat, false, "test/model", None);

    store_observation(&manager, "read", 0);
    let read = toolbox.run("read_page", "{}").await.text;
    assert!(read.contains("Example"), "{read}");
    assert!(manager.observation_registry().metadata("read").is_some());

    for (run_id, tool, args) in [
        ("browse", "browse", r#"{"url":"http://93.184.216.34/"}"#),
        ("click", "click", r#"{"text":"Go"}"#),
        (
            "type",
            "type_text",
            r#"{"selector":"input[name=q]","text":"hello"}"#,
        ),
    ] {
        manager.observation_registry().release("read");
        let generation = manager
            .desktop_state_registry()
            .for_bot("arthur")
            .lock()
            .await
            .generation();
        store_observation(&manager, run_id, generation);
        let result = toolbox.run(tool, args).await.text;
        assert!(!result.contains("did not answer"), "{tool}: {result}");
        assert!(
            manager.observation_registry().metadata(run_id).is_none(),
            "{tool} left a coordinate token valid"
        );
    }
    assert!(
        !mutation_saw_observation.load(Ordering::SeqCst),
        "a coordinate token was still present when CDP received a mutation"
    );
}

struct ShellDocker {
    observations: Arc<server::observations::ObservationRegistry>,
    mutation_saw_observation: AtomicBool,
    fail: bool,
    hold: bool,
    started: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait]
impl DockerRun for ShellDocker {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        match args.first() {
            Some(&"inspect") => DockerResult {
                ok: true,
                stdout: "running true".into(),
                stderr: String::new(),
            },
            Some(&"exec") => {
                if !self.observations.is_empty() {
                    self.mutation_saw_observation.store(true, Ordering::SeqCst);
                }
                self.started.notify_one();
                if self.hold {
                    let permit = self.release.acquire().await.expect("shell release open");
                    permit.forget();
                }
                DockerResult {
                    ok: !self.fail,
                    stdout: String::new(),
                    stderr: if self.fail {
                        "fixture shell failure".into()
                    } else {
                        String::new()
                    },
                }
            }
            other => panic!("unexpected docker call: {other:?}"),
        }
    }
}

fn shell_manager(
    docker_factory: impl FnOnce(Arc<server::observations::ObservationRegistry>) -> Arc<ShellDocker>,
) -> (Arc<RunManager>, Arc<ShellDocker>) {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    seed_bot(&db, "arthur", "Arthur");
    server::desk::ensure_desk_tables(&db.lock().unwrap()).unwrap();
    db.lock()
        .unwrap()
        .conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
             VALUES ('arthur', 'bullpen-vm-arthur', 9501, 6501, 'running', '2026-09-18T12:00:00Z')",
            [],
        )
        .unwrap();
    let observations = Arc::new(server::observations::ObservationRegistry::new());
    let desktop_states = Arc::new(server::observations::DesktopStateRegistry::new());
    let docker = docker_factory(Arc::clone(&observations));
    let manager = Arc::new(RunManager::with_shared_desktop_state(
        db,
        Arc::new(NeverCalledPort),
        sandbox::default_sandbox(),
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::new(config()),
        true,
        Arc::new(model::FixtureCatalog::from_json("[]").unwrap()),
        observations,
        desktop_states,
    ));
    (manager, docker)
}

#[tokio::test]
async fn failed_toolbox_shell_invalidates_before_docker_exec() {
    let (manager, docker) = shell_manager(|observations| {
        Arc::new(ShellDocker {
            observations,
            mutation_saw_observation: AtomicBool::new(false),
            fail: true,
            hold: false,
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    });
    store_observation(&manager, "shell-failure", 0);
    let result = manager
        .toolbox_for("arthur", Trigger::Chat, false, "test/model", None)
        .run("desk_shell", r#"{"command":"false"}"#)
        .await
        .text;
    assert!(result.contains("fixture shell failure"), "{result}");
    assert!(
        manager
            .observation_registry()
            .metadata("shell-failure")
            .is_none()
    );
    assert!(!docker.mutation_saw_observation.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancelled_toolbox_shell_keeps_desktop_lock_until_docker_exec_finishes() {
    let (manager, docker) = shell_manager(|observations| {
        Arc::new(ShellDocker {
            observations,
            mutation_saw_observation: AtomicBool::new(false),
            fail: false,
            hold: true,
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    });
    store_observation(&manager, "shell-cancel", 0);
    let toolbox = manager.toolbox_for("arthur", Trigger::Chat, false, "test/model", None);
    let started = docker.started.notified();
    tokio::pin!(started);
    let caller = tokio::spawn(async move {
        toolbox
            .run("desk_shell", r#"{"command":"sleep forever"}"#)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut started)
        .await
        .expect("shell exec started");
    caller.abort();

    let desktop = manager.desktop_state_registry().for_bot("arthur");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            desktop.clone().lock_owned()
        )
        .await
        .is_err(),
        "caller cancellation released the bot lock before docker exec ended"
    );
    assert!(
        manager
            .observation_registry()
            .metadata("shell-cancel")
            .is_none()
    );
    docker.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), desktop.lock_owned())
        .await
        .expect("shell worker released bot lock after docker completion");
}
