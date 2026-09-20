//! S7-04: namespaced connector tools offered in runs and dispatched via MCP.

mod common;

use async_trait::async_trait;
use common::{ScriptedPort, own_conversation, seed_bot, seed_user_message};
use model::ladder::Trigger;
use model::{MessageContent, ModelEvent, ModelMessage, ToolCall};
use reqwest::header::HeaderMap;
use serde_json::json;
use server::egress::Resolver;
use server::mcp::{ConnectorTool, McpHttpResponse, McpTransport};
use server::oauth::OAuthHttp;
use server::runs::{ConnectorHooks, RunEvent, RunManager, StartOptions};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use store::Db;

struct RuntimeMcpTransport;

#[async_trait]
impl McpTransport for RuntimeMcpTransport {
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
            "tools/call" => json!({
                "content": [{ "type": "text", "text": "connector hit: ok" }]
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

fn connector_tool_then_answer(tool: &str, arguments: &str) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: tool.to_string(),
                arguments: arguments.to_string(),
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

#[tokio::test]
async fn enabled_connector_tool_is_offered_and_dispatched() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "search mail");

    let connector_id = {
        let db = db.lock().expect("db");
        store::add_connector(&db, "Example Tools", "https://mcp.example.com/rpc", None)
            .expect("add connector")
            .connector
            .expect("connector row")
            .id
    };
    {
        let db = db.lock().expect("db");
        store::set_bot_connector(&db, "arthur", &connector_id, true).expect("enable");
    }

    let catalogue = Arc::new(Mutex::new(HashMap::from([(
        connector_id.clone(),
        vec![ConnectorTool {
            name: "search".to_string(),
            description: "Searches.".to_string(),
            input_schema: json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
        }],
    )])));

    let hooks = Arc::new(ConnectorHooks {
        catalogue,
        transport: Arc::new(RuntimeMcpTransport),
        resolver: Arc::new(PublicResolver),
        oauth_http: Arc::new(NoOAuthHttp),
    });

    let port = Arc::new(connector_tool_then_answer(
        "example_tools__search",
        r#"{"q":"hello"}"#,
    ));
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
        messages: vec![ModelMessage::user("search mail")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_done(manager.subscribe(&run_id)).await;

    let toolbox = manager.toolbox_for("arthur", Trigger::Chat, false, "test/model", None);
    assert!(
        toolbox
            .specs
            .iter()
            .any(|s| s.name == "example_tools__search"),
        "expected connector tool in offered specs"
    );

    let requests = port.requests();
    assert!(
        requests[0]
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|t| t.name == "example_tools__search")),
        "model should see namespaced connector tool"
    );

    assert_eq!(tool_result_text(&db, &run_id), "connector hit: ok");
}
