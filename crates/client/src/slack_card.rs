//! S5c-04: Settings > Slack card. Port of
//! `bullpen-night/src/client/SlackCard.tsx` - the paste form for the three
//! Slack app tokens, the connected-state summary, the answer-bot picker and
//! Disconnect. Widened past the TS original in one place: the TS card's
//! copy only names "the two secrets" (`slack-wizard.sh` is TS-only, not
//! ported - see this slice's Design bullet); this card instead spells out
//! all three fields plus the Events Request URL inline, since there is no
//! wizard here to say it for Josh.
//!
//! Not in `settings.rs` itself (a NEW file instead) per the ticket's file
//! ownership - `settings.rs` renders it as one more card inside
//! `GeneralSettings`.
//!
//! This module makes its own HTTP calls with `gloo_net::http::Request`
//! rather than adding functions to `api.rs` - that file is not in this
//! ticket's owned files (S5b-F rule: one owner per file, and the orchestrator
//! lands cross-file wiring). `fetch_bot_roster` below is a deliberate
//! near-duplicate of `app.rs`'s own private `fetch_roster` (same `GET
//! /api/roster`, same `Roster` type from `types.rs`) for the same reason -
//! that function is private to `app.rs`, which this ticket does not own
//! either. Same posture `routes/slack.rs` itself took with `history_turns`/
//! `regex_matches_or_fails_open` in S5c-03 (see that file's own doc).

use crate::message_time::format_time;
use crate::transport::{Request, Response};
use crate::types::{Bot, Roster};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// `GET /api/slack`'s response shape (`crates/server/src/slack.rs::
/// SlackStatus`) - camelCase over the wire, never a token field, matched
/// field-for-field.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SlackStatus {
    configured: bool,
    team_name: Option<String>,
    bot_user_id: Option<String>,
    connected_at: Option<String>,
    has_app_token: bool,
    answer_bot_id: Option<String>,
    answer_bot_name: Option<String>,
    /// S5c-F-03 (F10, client half): the Events Request URL to paste into
    /// Slack's app settings, `None` when the server's `PUBLIC_URL` is unset.
    events_url: Option<String>,
}

#[derive(Deserialize)]
struct SlackError {
    error: String,
}

async fn slack_error(resp: Response, url: &str) -> String {
    let status = resp.status();
    match resp.json::<SlackError>().await {
        Ok(body) => body.error,
        Err(_) => format!("{url} -> {status}"),
    }
}

async fn fetch_slack_status() -> Result<SlackStatus, String> {
    let resp = Request::get("/api/slack")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(slack_error(resp, "/api/slack").await);
    }
    resp.json::<SlackStatus>().await.map_err(|e| e.to_string())
}

/// Near-duplicate of `app.rs`'s private `fetch_roster` - see this module's
/// own doc for why it is not reused. A failed fetch leaves the answer-bot
/// select simply empty rather than surfacing a second error alongside
/// whatever the Slack card itself is already showing.
async fn fetch_bot_roster() -> Vec<Bot> {
    let Ok(resp) = Request::get("/api/roster").send().await else {
        return Vec::new();
    };
    if !resp.ok() {
        return Vec::new();
    }
    resp.json::<Roster>()
        .await
        .map(|r| r.bots)
        .unwrap_or_default()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectBody<'a> {
    bot_token: &'a str,
    signing_secret: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    app_token: Option<&'a str>,
}

/// `PUT /api/slack`. Proves the bot token with `auth.test` server-side
/// before storing anything - a typo'd token never lands here looking
/// connected (`crates/server/src/slack.rs::connect_slack`'s own doc).
async fn connect(
    bot_token: &str,
    signing_secret: &str,
    app_token: &str,
) -> Result<SlackStatus, String> {
    let app_token = app_token.trim();
    let body = ConnectBody {
        bot_token,
        signing_secret,
        app_token: if app_token.is_empty() {
            None
        } else {
            Some(app_token)
        },
    };
    let resp = Request::put("/api/slack")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(slack_error(resp, "/api/slack").await);
    }
    resp.json::<SlackStatus>().await.map_err(|e| e.to_string())
}

/// `DELETE /api/slack`. The route's own success body is `{"ok": true}`, not
/// a `SlackStatus` - the caller re-fetches status after this returns rather
/// than trust a shape this function does not parse.
async fn disconnect() -> Result<(), String> {
    let resp = Request::delete("/api/slack")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        Ok(())
    } else {
        Err(slack_error(resp, "/api/slack").await)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerBotBody<'a> {
    bot_id: &'a str,
}

/// `PUT /api/slack/answer-bot`.
async fn set_answer_bot(bot_id: &str) -> Result<SlackStatus, String> {
    let resp = Request::put("/api/slack/answer-bot")
        .json(&AnswerBotBody { bot_id })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(slack_error(resp, "/api/slack/answer-bot").await);
    }
    resp.json::<SlackStatus>().await.map_err(|e| e.to_string())
}

/// The Slack card - one more `.stg-sub` inside `settings.rs`'s
/// `GeneralSettings`, same shell every other card there uses.
#[component]
pub fn SlackCard() -> Element {
    let mut status = use_signal(|| None::<SlackStatus>);
    let mut bots = use_signal(Vec::<Bot>::new);
    let mut bot_token = use_signal(String::new);
    let mut signing_secret = use_signal(String::new);
    let mut app_token = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    use_effect(move || {
        spawn(async move {
            if let Ok(s) = fetch_slack_status().await {
                status.set(Some(s));
            }
        });
        spawn(async move {
            bots.set(fetch_bot_roster().await);
        });
    });

    let do_connect = move |_| {
        let token = bot_token.read().trim().to_string();
        let secret = signing_secret.read().trim().to_string();
        let app = app_token.read().clone();
        busy.set(true);
        error.set(None);
        spawn(async move {
            match connect(&token, &secret, &app).await {
                Ok(s) => {
                    bot_token.set(String::new());
                    signing_secret.set(String::new());
                    app_token.set(String::new());
                    status.set(Some(s));
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    let do_disconnect = move |_| {
        busy.set(true);
        error.set(None);
        spawn(async move {
            match disconnect().await {
                Ok(()) => {
                    if let Ok(s) = fetch_slack_status().await {
                        status.set(Some(s));
                    }
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    let on_answer_bot = move |evt: FormEvent| {
        let id = evt.value();
        spawn(async move {
            if let Ok(s) = set_answer_bot(&id).await {
                status.set(Some(s));
            }
        });
    };

    let is_busy = *busy.read();
    let configured = status
        .read()
        .as_ref()
        .map(|s| s.configured)
        .unwrap_or(false);
    let can_connect =
        !is_busy && !bot_token.read().trim().is_empty() && !signing_secret.read().trim().is_empty();

    rsx! {
        div { class: "stg-sub", "data-slot": "slack",
            h4 { class: "stg-sub-h", "Slack" }
            p { class: "set-note",
                "A DM to the app, or an @mention in a channel, runs a bot and replies in place. A routine can also trigger on a Slack mention, keyword, message or reaction. From your Slack app's settings page, paste in the Bot User OAuth Token, the Signing Secret, and - only if you're using Socket Mode - the App-Level Token. Set the app's Events Request URL to "
                code {
                    if let Some(url) = status.read().as_ref().and_then(|s| s.events_url.as_ref()) {
                        "{url}"
                    } else {
                        "<this site's origin>/api/slack/events"
                    }
                }
                ". Slack must be able to reach that URL from the internet."
            }

            if let Some(err) = error.read().clone() {
                div { class: "refusal", role: "alert",
                    b { "Refused." }
                    p { "{err}" }
                }
            }

            if configured {
                if let Some(s) = status.read().clone() {
                    div { class: "slack-connected",
                        p { class: "slack-conn-state",
                            "Connected as "
                            {s.team_name.clone().unwrap_or_else(|| "a workspace".to_string())}
                        }
                        p { class: "muted",
                            "Bot user "
                            code { "{s.bot_user_id.clone().unwrap_or_else(|| \"?\".to_string())}" }
                            if s.has_app_token {
                                " · Socket Mode token on file"
                            }
                            if let Some(at) = s.connected_at.clone() {
                                if !at.is_empty() {
                                    " · connected {format_time(&at)}"
                                }
                            }
                        }
                        div { class: "field",
                            span { "Which bot answers in Slack" }
                            select {
                                "aria-label": "Slack answer bot",
                                value: "{s.answer_bot_id.clone().unwrap_or_default()}",
                                onchange: on_answer_bot,
                                for bot in bots.read().iter() {
                                    option { key: "{bot.id}", value: "{bot.id}", "{bot.name}" }
                                }
                            }
                        }
                        if let Some(name) = s.answer_bot_name.clone() {
                            p { class: "muted", "Currently {name}." }
                        }
                        div { class: "routine-acts",
                            button {
                                class: "danger",
                                disabled: is_busy,
                                onclick: do_disconnect,
                                "Disconnect"
                            }
                        }
                    }
                }
            } else {
                div { class: "slack-card-form",
                    input {
                        r#type: "password",
                        value: "{bot_token}",
                        placeholder: "Bot token (xoxb-…)",
                        "aria-label": "Slack bot token",
                        oninput: move |e| bot_token.set(e.value()),
                    }
                    input {
                        r#type: "password",
                        value: "{signing_secret}",
                        placeholder: "Signing secret",
                        "aria-label": "Slack signing secret",
                        oninput: move |e| signing_secret.set(e.value()),
                    }
                    input {
                        r#type: "password",
                        value: "{app_token}",
                        placeholder: "App-level token (xapp-…) - only for Socket Mode",
                        "aria-label": "Slack app-level token",
                        oninput: move |e| app_token.set(e.value()),
                    }
                    button {
                        class: "stg-btn",
                        disabled: !can_connect,
                        onclick: do_connect,
                        if is_busy { "Connecting…" } else { "Connect" }
                    }
                }
            }
        }
    }
}
