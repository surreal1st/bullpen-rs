//! S7-05: `list_resources` / `read_resource` in runs.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{ScriptedPort, own_conversation, seed_bot, seed_user_message};
use model::ladder::Trigger;
use model::{MessageContent, ModelEvent, ModelMessage, ToolCall};
use reqwest::header::HeaderMap;
use serde_json::json;
use server::egress::Resolver;
use server::mcp::{McpHttpResponse, McpTransport};
use server::oauth::OAuthHttp;
use server::runs::{ConnectorHooks, RunEvent, RunManager, StartOptions};
use store::Db;

struct ResourceMcpTransport;

#[async_trait]
impl McpTransport for ResourceMcpTransport {
    async fn post(
        &self,
        _url: &str,
        _headers: HeaderMap,
        body: String,
    ) -> Result<McpHttpResponse, String> {
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
        let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = parsed.get("id").cloned().unwrap_or(json!("1"));
        let result = match method {
            "initialize" => json!({ "protocolVersion": "2025-06-18" }),
            "resources/list" => json!({
                "resources": [
                    { "uri": "file://one.txt", "name": "File One", "mimeType": "text/plain" }
                ]
            }),
            "resources/read" => json!({
                "contents": [{ "uri": "file://one.txt", "text": "Hello from the resource!" }]
            }),
            _ => json!({}),
        };
        let payload = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
        Ok(McpHttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: payload,
        })
    }
}

struct PublicResolver;

#[async_trait]
impl Resolver for PublicResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Ok(vec!["93.184.216.34".to_string()])
    }
}

struct NoOAuthHttp;

#[async_trait]
impl OAuthHttp for NoOAuthHttp {
    async fn get_json(&self, _url: &str) -> Result<(u16, serde_json::Value), String> {
        Err("unused".to_string())
    }

    async fn post_json(
        &self,
        _url: &str,
        _body: serde_json::Value,
    ) -> Result<(u16, serde_json::Value), String> {
        Err("unused".to_string())
    }

    async fn post_form(
        &self,
        _url: &str,
        _form: &[(&str, String)],
    ) -> Result<(u16, serde_json::Value), String> {
        Err("unused".to_string())
    }
}

fn as_port_arc(port: &Arc<ScriptedPort>) -> Arc<dyn model::ModelPort> {
    Arc::clone(port) as Arc<dyn model::ModelPort>
}

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open db");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing off");
    server::judge::set_judge_enabled(&db, false).expect("judge off");
    Arc::new(Mutex::new(db))
}

fn tool_result_text(db: &Arc<Mutex<Db>>, run_id: &str) -> String {
    let db = db.lock().expect("db");
    let messages_json: String = db
        .conn()
        .query_row(
            "SELECT messages FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("messages");
    let messages: Vec<ModelMessage> = serde_json::from_str(&messages_json).expect("parse");
    let tool_result = messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool result");
    let MessageContent::Text(text) = &tool_result.content else {
        panic!("expected text");
    };
    text.clone()
}

async fn drain_until_done(mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>) {
    while let Some(event) = rx.recv().await {
        if matches!(event, RunEvent::Done { .. } | RunEvent::Error { .. }) {
            break;
        }
    }
}

fn read_resource_script() -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "read_resource".to_string(),
                arguments: r#"{"connector":"Example Tools","uri":"file://one.txt"}"#.to_string(),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "Done.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ])
}

#[tokio::test]
async fn read_resource_dispatches_to_mcp() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "read that file");

    let connector_id = {
        let db = db.lock().expect("db");
        store::add_connector(&db, "Example Tools", "https://mcp.example.com/rpc", None)
            .expect("add")
            .connector
            .expect("row")
            .id
    };
    {
        let db = db.lock().expect("db");
        store::set_bot_connector(&db, "arthur", &connector_id, true).expect("enable");
    }

    let hooks = Arc::new(ConnectorHooks {
        catalogue: Arc::new(Mutex::new(std::collections::HashMap::new())),
        transport: Arc::new(ResourceMcpTransport),
        resolver: Arc::new(PublicResolver),
        oauth_http: Arc::new(NoOAuthHttp),
    });

    let port = Arc::new(read_resource_script());
    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        as_port_arc(&port),
        Arc::new(server::sandbox::UnavailableSandbox::new("test")),
    ));
    manager.set_connector_hooks(hooks);

    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("read that file")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_done(manager.subscribe(&run_id)).await;

    assert_eq!(tool_result_text(&db, &run_id), "Hello from the resource!");
}
