//! S7-07: Settings > Connectors — port of
//! `bullpen-night/src/client/Connectors.tsx`. Lists MCP connectors, OAuth
//! Connect / Disconnect, add/remove rows, and optional tool preview. HTTP
//! lives here (same posture as `slack_card.rs`), not in `api.rs`.

use crate::transport::{Request, Response, open_view};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connector {
    id: String,
    name: String,
    url: String,
    created_at: String,
}

#[derive(Deserialize)]
struct ConnectorsList {
    connectors: Vec<Connector>,
}

#[derive(Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthState {
    configured: bool,
    connected: bool,
    issuer: Option<String>,
    scope: Option<String>,
    redirect_uri: String,
}

#[derive(Deserialize)]
struct ApiError {
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectResponse {
    authorize_url: Option<String>,
    already_open: Option<bool>,
    error: Option<String>,
    needs_client_credentials: Option<bool>,
    redirect_uri: Option<String>,
    issuer: Option<String>,
    message: Option<String>,
}

#[derive(Clone, PartialEq)]
struct NeedsClient {
    redirect_uri: String,
    issuer: String,
}

#[derive(Clone, PartialEq, Deserialize)]
struct Tool {
    name: String,
    description: String,
}

#[derive(Deserialize)]
struct ToolsBody {
    tools: Option<Vec<Tool>>,
    error: Option<String>,
}

#[derive(Serialize)]
struct AddConnectorBody<'a> {
    name: &'a str,
    url: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectCredentials<'a> {
    client_id: &'a str,
    client_secret: &'a str,
}

async fn api_error(resp: Response, url: &str) -> String {
    let status = resp.status();
    match resp.json::<ApiError>().await {
        Ok(body) => body.error.unwrap_or_else(|| format!("{url} -> {status}")),
        Err(_) => format!("{url} -> {status}"),
    }
}

async fn fetch_connectors() -> Result<Vec<Connector>, String> {
    let resp = Request::get("/api/connectors")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(api_error(resp, "/api/connectors").await);
    }
    resp.json::<ConnectorsList>()
        .await
        .map(|b| b.connectors)
        .map_err(|e| e.to_string())
}

async fn add_connector(name: &str, url: &str) -> Result<(), String> {
    let resp = Request::post("/api/connectors")
        .json(&AddConnectorBody { name, url })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(api_error(resp, "/api/connectors").await)
    }
}

async fn remove_connector(id: &str) -> Result<(), String> {
    let url = format!("/api/connectors/{id}");
    let resp = Request::delete(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(api_error(resp, &url).await)
    }
}

async fn fetch_auth(id: &str) -> Option<AuthState> {
    let url = format!("/api/connectors/{id}/auth");
    let resp = Request::get(&url).send().await.ok()?;
    if !resp.ok() {
        return None;
    }
    resp.json::<AuthState>().await.ok()
}

async fn delete_auth(id: &str) -> Result<(), String> {
    let url = format!("/api/connectors/{id}/auth");
    let resp = Request::delete(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(api_error(resp, &url).await)
    }
}

async fn post_connect(id: &str, creds: Option<(&str, &str)>) -> Result<ConnectResponse, String> {
    let url = format!("/api/connectors/{id}/connect");
    let resp = if let Some((client_id, client_secret)) = creds {
        Request::post(&url)
            .json(&ConnectCredentials {
                client_id,
                client_secret,
            })
            .map_err(|e| e.to_string())?
    } else {
        Request::post(&url)
            .json(&serde_json::json!({}))
            .map_err(|e| e.to_string())?
    }
    .send()
    .await
    .map_err(|e| e.to_string())?;
    let body = resp
        .json::<ConnectResponse>()
        .await
        .map_err(|e| e.to_string())?;
    Ok(body)
}

enum ConnectOutcome {
    NeedsClient(NeedsClient),
    AlreadyOpen(String),
    OpenedAuth,
    Problem(String),
}

async fn start_connector_connect(
    id: &str,
    creds: Option<(&str, &str)>,
) -> Result<ConnectOutcome, String> {
    let body = post_connect(id, creds).await?;
    if body.needs_client_credentials == Some(true) {
        return Ok(ConnectOutcome::NeedsClient(NeedsClient {
            redirect_uri: body.redirect_uri.unwrap_or_default(),
            issuer: body.issuer.unwrap_or_default(),
        }));
    }
    if body.already_open == Some(true) {
        return Ok(ConnectOutcome::AlreadyOpen(
            body.message
                .or(body.error)
                .unwrap_or_else(|| "That connector needs no authorization.".to_string()),
        ));
    }
    if let Some(url) = body.authorize_url {
        open_view(&url);
        return Ok(ConnectOutcome::OpenedAuth);
    }
    Ok(ConnectOutcome::Problem(body.error.unwrap_or_else(|| {
        "Connect did not return an authorization URL.".to_string()
    })))
}

async fn fetch_tools(id: &str) -> Result<Vec<Tool>, String> {
    let url = format!("/api/connectors/{id}/tools");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    let body = resp.json::<ToolsBody>().await.map_err(|e| e.to_string())?;
    if body.tools.is_some() {
        Ok(body.tools.unwrap_or_default())
    } else {
        Err(body
            .error
            .unwrap_or_else(|| "could not reach that connector".to_string()))
    }
}

/// Connectors block inside Settings → Connectors (above Slack + marketplace).
#[component]
pub fn ConnectorsCard() -> Element {
    let mut connectors = use_signal(Vec::<Connector>::new);
    let mut name = use_signal(String::new);
    let mut url = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);

    let refresh = move |_| {
        spawn(async move {
            match fetch_connectors().await {
                Ok(list) => connectors.set(list),
                Err(e) => error.set(Some(e)),
            }
        });
    };

    use_effect(move || {
        spawn(async move {
            if let Ok(list) = fetch_connectors().await {
                connectors.set(list);
            }
        });
    });

    let on_add = move |_| {
        let n = name.read().trim().to_string();
        let u = url.read().trim().to_string();
        if n.is_empty() || u.is_empty() {
            return;
        }
        error.set(None);
        spawn(async move {
            match add_connector(&n, &u).await {
                Ok(()) => {
                    name.set(String::new());
                    url.set(String::new());
                    if let Ok(list) = fetch_connectors().await {
                        connectors.set(list);
                    }
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    let can_add = !name.read().trim().is_empty() && !url.read().trim().is_empty();
    let count = connectors.read().len();

    rsx! {
        div { class: "stg-sub", "data-slot": "connectors",
            h4 { class: "stg-sub-h", "MCP connectors ", i { "{count}" } }

            if let Some(err) = error.read().clone() {
                div { class: "refusal", role: "alert",
                    b { "Refused." }
                    p { "{err}" }
                }
            }

            if count == 0 {
                p { class: "muted",
                    "Nothing added yet. A connector is an MCP server: Gmail, Calendar, or anything else that speaks the protocol."
                }
            }

            for connector in connectors.read().iter() {
                ConnectorRow {
                    key: "{connector.id}",
                    connector: connector.clone(),
                    on_changed: refresh,
                }
            }

            div { class: "conn-new",
                input {
                    value: "{name}",
                    placeholder: "What to call it",
                    "aria-label": "Connector name",
                    oninput: move |e| name.set(e.value()),
                }
                input {
                    value: "{url}",
                    placeholder: "https://mcp.example.com/mcp",
                    "aria-label": "Connector URL",
                    oninput: move |e| url.set(e.value()),
                }
                button {
                    disabled: !can_add,
                    onclick: on_add,
                    "Add connector"
                }
            }
        }
    }
}

#[component]
fn ConnectorRow(connector: Connector, on_changed: EventHandler<()>) -> Element {
    let id = connector.id.clone();
    let mut auth = use_signal(|| None::<AuthState>);
    let mut tools = use_signal(|| None::<Vec<Tool>>);
    let mut busy = use_signal(|| false);
    let mut problem = use_signal(|| None::<String>);
    let mut needs_client = use_signal(|| None::<NeedsClient>);
    let mut client_id = use_signal(String::new);
    let mut client_secret = use_signal(String::new);

    use_effect({
        let id = id.clone();
        move || {
            let id = id.clone();
            spawn(async move {
                auth.set(fetch_auth(&id).await);
            });
        }
    });

    let on_connect = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            busy.set(true);
            problem.set(None);
            spawn(async move {
                match start_connector_connect(&id, None).await {
                    Ok(ConnectOutcome::NeedsClient(nc)) => needs_client.set(Some(nc)),
                    Ok(ConnectOutcome::AlreadyOpen(msg)) => problem.set(Some(msg)),
                    Ok(ConnectOutcome::OpenedAuth) => {
                        needs_client.set(None);
                        auth.set(fetch_auth(&id).await);
                    }
                    Ok(ConnectOutcome::Problem(msg)) => problem.set(Some(msg)),
                    Err(e) => problem.set(Some(e)),
                }
                busy.set(false);
            });
        }
    };

    let on_continue_oauth = {
        let id = id.clone();
        move |_| {
            let cid = client_id.read().trim().to_string();
            let secret = client_secret.read().trim().to_string();
            if cid.is_empty() {
                return;
            }
            let id = id.clone();
            busy.set(true);
            problem.set(None);
            spawn(async move {
                match start_connector_connect(&id, Some((&cid, &secret))).await {
                    Ok(ConnectOutcome::NeedsClient(nc)) => needs_client.set(Some(nc)),
                    Ok(ConnectOutcome::AlreadyOpen(msg)) => problem.set(Some(msg)),
                    Ok(ConnectOutcome::OpenedAuth) => {
                        needs_client.set(None);
                        auth.set(fetch_auth(&id).await);
                    }
                    Ok(ConnectOutcome::Problem(msg)) => problem.set(Some(msg)),
                    Err(e) => problem.set(Some(e)),
                }
                busy.set(false);
            });
        }
    };

    let on_disconnect = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            spawn(async move {
                if delete_auth(&id).await.is_ok() {
                    auth.set(fetch_auth(&id).await);
                }
            });
        }
    };

    let on_remove = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            spawn(async move {
                let _ = remove_connector(&id).await;
                on_changed.call(());
            });
        }
    };

    let on_show_tools = {
        let id = id.clone();
        move |_| {
            let id = id.clone();
            busy.set(true);
            problem.set(None);
            spawn(async move {
                match fetch_tools(&id).await {
                    Ok(list) => tools.set(Some(list)),
                    Err(e) => {
                        problem.set(Some(e));
                        tools.set(Some(Vec::new()));
                    }
                }
                busy.set(false);
            });
        }
    };

    let is_busy = *busy.read();
    let connected = auth.read().as_ref().map(|a| a.connected).unwrap_or(false);
    let configured = auth.read().as_ref().map(|a| a.configured).unwrap_or(false);
    let state_label = if connected {
        "Connected"
    } else if configured {
        "Not authorized"
    } else {
        "No auth"
    };

    rsx! {
        article { class: "conn",
            div { class: "conn-top",
                b { "{connector.name}" }
                span {
                    class: if connected { "conn-state is-on" } else { "conn-state" },
                    "{state_label}"
                }
            }
            p { class: "conn-url mono", "{connector.url}" }

            if let Some(msg) = problem.read().clone() {
                div { class: "caution",
                    b { "Heads up." }
                    p { "{msg}" }
                }
            }

            if let Some(nc) = needs_client.read().clone() {
                div { class: "conn-manual",
                    p {
                        b { "{nc.issuer}" }
                        " does not hand out clients automatically. Create an OAuth client there, set its redirect URI to exactly this, then paste the id and secret back:"
                    }
                    code { class: "conn-redirect", "{nc.redirect_uri}" }
                    input {
                        value: "{client_id}",
                        placeholder: "Client ID",
                        "aria-label": "OAuth client ID",
                        oninput: move |e| client_id.set(e.value()),
                    }
                    input {
                        r#type: "password",
                        value: "{client_secret}",
                        placeholder: "Client secret",
                        "aria-label": "OAuth client secret",
                        oninput: move |e| client_secret.set(e.value()),
                    }
                    button {
                        disabled: client_id.read().trim().is_empty(),
                        onclick: on_continue_oauth,
                        "Continue"
                    }
                }
            }

            if let Some(list) = tools.read().clone() {
                div { class: "conn-tools",
                    if list.is_empty() {
                        p { class: "muted", "No tools came back." }
                    }
                    for tool in list.iter() {
                        div { key: "{tool.name}", class: "conn-tool",
                            code { "{tool.name}" }
                            span { "{tool.description}" }
                        }
                    }
                }
            }

            div { class: "conn-acts",
                if connected {
                    button { onclick: on_disconnect, "Disconnect" }
                } else {
                    button {
                        class: "primary",
                        disabled: is_busy,
                        onclick: on_connect,
                        if is_busy { "Working…" } else { "Connect" }
                    }
                }
                button {
                    disabled: is_busy,
                    onclick: on_show_tools,
                    if tools.read().is_none() { "Show tools" } else { "Refresh tools" }
                }
                button {
                    class: "danger",
                    onclick: on_remove,
                    "Remove"
                }
            }
        }
    }
}
