//! S7-05: `list_resources` / `read_resource` — port of `app.ts` ~4990 / ~5983.

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::ConnectorFull;
use store::Db;

use crate::mcp::{self, McpCallOptions};
use crate::runs::ConnectorHooks;

use super::lock_db;

pub fn list_resources_spec() -> ToolSpec {
    ToolSpec {
        name: "list_resources".to_string(),
        description: "List resources from a connector that is switched on for you. Resources are \
documents or data the connector can read. Returns uri, name, and optional description or MIME \
type for each."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "connector": {
                    "type": "string",
                    "description": "The connector's name, such as Gmail or Google Drive."
                }
            },
            "required": ["connector"]
        }),
    }
}

pub fn read_resource_spec() -> ToolSpec {
    ToolSpec {
        name: "read_resource".to_string(),
        description: "Read the text content of one resource from a connector. Pass the uri from \
list_resources. Returns the text, optionally with MIME type."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "connector": { "type": "string", "description": "The connector's name." },
                "uri": {
                    "type": "string",
                    "description": "The resource's uri, from list_resources."
                }
            },
            "required": ["connector", "uri"]
        }),
    }
}

#[derive(Deserialize)]
struct ListArgs {
    connector: Option<String>,
}

#[derive(Deserialize)]
struct ReadArgs {
    connector: Option<String>,
    uri: Option<String>,
}

fn find_enabled_connector(
    db: &Arc<std::sync::Mutex<Db>>,
    bot_id: &str,
    connector_name: &str,
) -> Option<ConnectorFull> {
    let db = lock_db(db);
    let enabled = store::connectors_for_bot(&db, bot_id).unwrap_or_default();
    let want = mcp::slug_name(connector_name);
    enabled
        .into_iter()
        .find(|c| mcp::slug_name(&c.name) == want)
}

fn auth_message(connector: &ConnectorFull) -> String {
    format!(
        "That connector is not authorized yet. Tell Josh to open Connectors and press Connect on {}. Do not retry.",
        connector.name
    )
}

fn fail_message(error: Option<String>) -> String {
    format!(
        "The connector failed: {}",
        error.unwrap_or_else(|| "unknown error".to_string())
    )
}

pub async fn run_list_resources(
    db: &Arc<std::sync::Mutex<Db>>,
    bot_id: &str,
    args: &str,
    hooks: &ConnectorHooks,
) -> String {
    let parsed: ListArgs = serde_json::from_str(args).unwrap_or(ListArgs { connector: None });
    let connector_name = parsed.connector.unwrap_or_default();
    let Some(connector) = find_enabled_connector(db, bot_id, &connector_name) else {
        return "That connector is not switched on for you.".to_string();
    };
    let bearer =
        crate::oauth::bearer_for_shared(db, &connector.id, hooks.oauth_http.as_ref()).await;
    let listed = mcp::list_connector_resources(
        &connector,
        McpCallOptions {
            transport: hooks.transport.as_ref(),
            resolver: hooks.resolver.as_ref(),
            bearer: bearer.as_deref(),
        },
    )
    .await;
    if !listed.ok {
        if listed.needs_auth {
            return auth_message(&connector);
        }
        return fail_message(listed.error);
    }
    if listed.resources.is_empty() {
        return "The connector has no resources.".to_string();
    }
    listed
        .resources
        .iter()
        .map(|r| {
            let mime = r
                .mime_type
                .as_ref()
                .map(|m| format!("  ({m})"))
                .unwrap_or_default();
            format!("- {}  {}{}", r.uri, r.name, mime)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn run_read_resource(
    db: &Arc<std::sync::Mutex<Db>>,
    bot_id: &str,
    args: &str,
    hooks: &ConnectorHooks,
) -> String {
    let parsed: ReadArgs = serde_json::from_str(args).unwrap_or(ReadArgs {
        connector: None,
        uri: None,
    });
    let connector_name = parsed.connector.unwrap_or_default();
    let uri = parsed.uri.unwrap_or_default();
    if uri.trim().is_empty() {
        return "No resource uri was given.".to_string();
    }
    let Some(connector) = find_enabled_connector(db, bot_id, &connector_name) else {
        return "That connector is not switched on for you.".to_string();
    };
    let bearer =
        crate::oauth::bearer_for_shared(db, &connector.id, hooks.oauth_http.as_ref()).await;
    let read = mcp::read_connector_resource(
        &connector,
        uri.trim(),
        McpCallOptions {
            transport: hooks.transport.as_ref(),
            resolver: hooks.resolver.as_ref(),
            bearer: bearer.as_deref(),
        },
    )
    .await;
    if !read.ok {
        if read.needs_auth {
            return auth_message(&connector);
        }
        return fail_message(read.error);
    }
    read.text.unwrap_or_default()
}
