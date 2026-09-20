//! S6-VM-01: the bot's own computer, live, at the top of its panel -
//! ported from `projects/bullpen-gate/src/client/VmCard.tsx` (178 lines).
//! Mounted by `thread.rs`'s `ChatPane`, which is this client's stand-in for
//! the TS `BotPanel`: every other section that file's own panel holds
//! (Permissions, Memory, Routines, Goals) already collapsed into a modal
//! opened from `pane-head` here rather than a persistent side list, so the
//! top of "the bot panel" in this client is the top of `ChatPane`'s own
//! `.pane`, right under that header.
//!
//! Three rules the TS original's own comments carry, kept here:
//!
//! 1. **Never starts a machine on its own.** Opening a bot's panel is not
//!    the same as that bot needing a computer - fifteen open panels must
//!    not become fifteen desktops. `api::ensure_vm` is called from exactly
//!    one place below: the `wake` closure bound to the "Start it"/"Wake
//!    it" button's `onclick`. The mount effect and the poll loop both
//!    drive [`run_cycle`] instead, whose own signature has no parameter
//!    capable of reaching `ensure_vm` at all - see that function's doc.
//!    S6-VM-01's bite (b).
//! 2. **The thumbnail is fetched only while the machine is live.** Each
//!    fetch is an ffmpeg process on meridian; pulling one for a sleeping
//!    or nonexistent machine burns that for a picture nobody is looking
//!    at. [`run_cycle`] only calls its thumbnail fetcher when [`is_live`]
//!    says yes - S6-VM-01's bite (c). (The TS original also pauses on a
//!    hidden browser TAB, via `document.visibilityState`; this client has
//!    no tabs, only a mounted-or-not panel, and closing the panel already
//!    stops this component's loop entirely - see the mount effect's own
//!    doc on why. Judgment call, named in `## Results`.)
//! 3. **The same answer as last time must not re-render the card.**
//!    `vm.peek()`-then-compare before `vm.set(...)` at the call site below,
//!    the same idiom `thread.rs`'s own conversation fetch already uses -
//!    `VmState` derives `PartialEq` for exactly this.
//!
//! 🔴 **The desktop-401 trap, named up front in the ticket.** The desktop
//! build signs in Rust-side and carries a Bearer token on every
//! `transport::Request` (`transport/native.rs`, S13b-01); the webview
//! itself has no session cookie. A plain
//! `<img src="/api/bots/:id/vm/thumbnail.png">` would be the webview
//! fetching that resource directly, outside this transport entirely -
//! unauthenticated, and a 401 on the one client Josh actually runs, while
//! looking fine on the web build (whose same-origin `fetch` cookie the
//! `<img>` tag happens to inherit too).
//!
//! **Chosen fix, and why:** `api::fetch_vm_thumbnail` reads the PNG as
//! bytes through the same `transport::Request` every other call in this
//! client already uses, and this file base64-encodes them into a
//! `data:image/png;base64,...` URI for the `<img>` tag's `src`. The
//! browser/webview never makes a second request of its own for a `data:`
//! URI, so there is nothing left that could go out unauthenticated. The
//! alternative considered was a signed, short-lived query-string token
//! appended to a plain `<img src>` (what some proxies do); rejected
//! because it is a second auth mechanism to keep in sync with the Bearer
//! one `transport::native` already carries, for a client that already has
//! a perfectly good authenticated transport sitting right there. Proven in
//! the DESKTOP build specifically by `transport::native`'s own bite (d)
//! test (`desktop_requests_carry_the_stored_bearer_token`) and by a
//! screenshot of the built desktop app - see this ticket's `## Results`.

use crate::api;
use crate::types::VmState;
use dioxus::prelude::*;
use std::future::Future;

const REFRESH_MS: u32 = 5_000;

#[component]
pub fn VmCard(bot_id: String, bot_name: String) -> Element {
    let mut vm = use_signal(|| None::<VmState>);
    let mut frame = use_signal(|| 0u64);
    let mut shot_failed = use_signal(|| false);
    let mut thumbnail = use_signal(|| None::<String>);
    let mut waking = use_signal(|| false);

    // Mount: an immediate cycle, then one every `REFRESH_MS` for as long as
    // this component stays mounted. `dioxus::prelude::spawn`'s future is
    // dropped when this component's scope unmounts (unlike
    // `crate::transport::spawn_task`, which deliberately targets
    // `ScopeId::ROOT` for callers with no live scope - see that function's
    // own doc) - so switching away from this bot (a fresh `key` on
    // `ChatPane`, per that component's own doc) is what stops the polling.
    let mount_bot_id = bot_id.clone();
    use_effect(move || {
        let bot_id = mount_bot_id.clone();
        vm.set(None);
        frame.set(0);
        shot_failed.set(false);
        thumbnail.set(None);
        spawn(async move {
            loop {
                let previous_frame = *frame.peek();
                let state_bot_id = bot_id.clone();
                let shot_bot_id = bot_id.clone();
                let (next, next_frame, shot) = run_cycle(
                    previous_frame,
                    || async move { api::fetch_vm(&state_bot_id).await },
                    |f| async move { api::fetch_vm_thumbnail(&shot_bot_id, f).await },
                )
                .await;

                // Rule 3: identical to what is already shown must not
                // re-render this card.
                if vm.peek().as_ref() != next.as_ref() {
                    vm.set(next);
                }
                if next_frame != previous_frame {
                    frame.set(next_frame);
                }
                // Any failure to get a picture - a 404/409/503 folded into
                // `Ok(None)` by `api::fetch_vm_thumbnail`, or a genuine
                // transport `Err` - reads as "booting", the same as the TS
                // original's `<img onError>` treats any failed image load
                // regardless of status. `None` (not live this tick) leaves
                // both signals exactly as they were.
                if let Some(shot) = shot {
                    if let Ok(Some(bytes)) = shot {
                        thumbnail.set(Some(to_data_uri(&bytes)));
                        if *shot_failed.peek() {
                            shot_failed.set(false);
                        }
                    } else if !*shot_failed.peek() {
                        shot_failed.set(true);
                    }
                }

                crate::transport::sleep(REFRESH_MS).await;
            }
        });
    });

    let wake_bot_id = bot_id.clone();
    let wake = move |_| {
        let bot_id = wake_bot_id.clone();
        waking.set(true);
        spawn(async move {
            if let Ok(next) = api::ensure_vm(&bot_id).await {
                vm.set(Some(next));
                shot_failed.set(false);
            }
            // The poll loop's next tick (at most `REFRESH_MS` away) says
            // what actually happened if this failed - same "let the state
            // poll cover it" posture the TS original's own `wake` takes.
            waking.set(false);
        });
    };

    let current = vm.read().clone();
    let live = is_live(current.as_ref());
    let show_shot = live && !*shot_failed.read();
    // 🔴 The URL a click opens comes from `viewPath` the API handed back,
    // never built by hand here - that path was a 404 for a week because
    // something DID build it by hand (`routes/vms.rs`'s own doc).
    let view_path = current.as_ref().and_then(|v| v.view_path.clone());
    let open = move |_| {
        if let Some(path) = view_path.clone() {
            crate::transport::open_view(&path);
        }
    };

    let wake_label = if *waking.read() {
        "Starting…"
    } else if current.as_ref().map(|v| v.state.as_str()) == Some("stopped") {
        "Wake it"
    } else {
        "Start it"
    };

    rsx! {
        section { class: "panel-sect vm-sect",
            div {
                class: if show_shot { "vm-screen is-live" } else { "vm-screen is-blank" },
                if show_shot {
                    button {
                        class: "vm-open",
                        title: "Open {bot_name}'s screen",
                        onclick: open,
                        if let Some(src) = thumbnail.read().clone() {
                            img { class: "vm-shot", src: "{src}", alt: "{bot_name}'s screen" }
                        }
                        span { class: "vm-open-hint", "Open" }
                    }
                } else {
                    div { class: "vm-blank",
                        {monitor_icon()}
                        p { class: "vm-blank-line", "{blank_line(current.as_ref(), *shot_failed.read(), &bot_name)}" }
                        if current.as_ref().map(|v| v.available) == Some(true) {
                            button {
                                class: "vm-wake",
                                disabled: *waking.read(),
                                onclick: wake,
                                "{wake_label}"
                            }
                        }
                    }
                }
            }

            p { class: "vm-caption",
                span { class: "vm-name", "{bot_name}\u{2019}s screen" }
                span { class: if live { "vm-dot is-on" } else { "vm-dot" } }
                span { class: "vm-state", "{state_word(current.as_ref(), *shot_failed.read())}" }
            }
        }
    }
}

fn monitor_icon() -> Element {
    rsx! {
        svg {
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.4",
            width: "26",
            height: "26",
            "aria-hidden": "true",
            rect { x: "2.5", y: "4", width: "19", height: "13", rx: "2" }
            path { d: "M9 20h6M12 17v3" }
        }
    }
}

/* ---------------------------------------------------------- pure functions */

/// A machine is "live" - a picture worth showing, a dot worth lighting -
/// only when the server says it is `available` AND its state is
/// `"running"` or `"starting"` (`crates/server/src/vm.rs`'s own state
/// machine: `"new"`/`"stopped"` are not live, `"none"`/`"unavailable"` are
/// the wrapper's own "no machine" answers - see `types::VmState`'s doc).
fn is_live(vm: Option<&VmState>) -> bool {
    matches!(vm, Some(v) if v.available && (v.state == "running" || v.state == "starting"))
}

/// Bite (c): bumps the thumbnail's cache-busting counter only when the
/// machine is actually live - `frame` is purely a cache-buster, and
/// bumping it for a sleeping or nonexistent machine has no picture to show
/// for it, only a wasted poll.
///
/// **Mutation for `## Results`:** replace the body with `current + 1`
/// unconditionally - `next_frame_holds_steady_when_not_live` goes red
/// immediately (the "not live" case is the only one that can tell the two
/// worlds apart; the "live" case gives the same answer either way).
fn next_frame(current: u64, live: bool) -> u64 {
    if live { current + 1 } else { current }
}

/// One poll cycle - the mount effect's first iteration and every
/// `REFRESH_MS` tick after both run this, and nothing else.
///
/// **This function's signature IS bite (b)'s guard.** There is no
/// parameter here that can reach `api::ensure_vm` - the only two
/// operations a cycle can perform are "read the state" and "read a
/// thumbnail frame, if the state just read says the machine is live". A
/// change that wants a poll cycle to also start a machine has to change
/// this function's SHAPE, not just a line inside it, which is what makes
/// rule 1 a structural property of the mount/poll path rather than a
/// promise to remember. `ensure_vm` is called from exactly one place in
/// this file: the `wake` closure bound to the button (see `VmCard`'s own
/// doc).
///
/// Generic over the two fetchers (rather than calling `api::fetch_vm`/
/// `api::fetch_vm_thumbnail` directly) so this is testable with in-memory
/// counting closures - no server, no env var, no global state - see this
/// module's `mod tests` for why: there is no Dioxus component test harness
/// anywhere in this codebase to mount `VmCard` itself and observe which
/// HTTP calls it made.
async fn run_cycle<S, SFut, T, TFut>(
    previous_frame: u64,
    fetch_state: S,
    fetch_thumbnail: T,
) -> (
    Option<VmState>,
    u64,
    Option<Result<Option<Vec<u8>>, String>>,
)
where
    S: FnOnce() -> SFut,
    SFut: Future<Output = Result<VmState, String>>,
    T: FnOnce(u64) -> TFut,
    TFut: Future<Output = Result<Option<Vec<u8>>, String>>,
{
    // Ported from the TS `load`'s own `catch { setVm(null) }`: a failed
    // read is treated as "nothing", not "keep showing the last answer".
    let next = fetch_state().await.ok();
    let live = is_live(next.as_ref());
    let frame = next_frame(previous_frame, live);
    let shot = if live {
        Some(fetch_thumbnail(frame).await)
    } else {
        None
    };
    (next, frame, shot)
}

/// What the empty screen says. Every branch is a different thing to do
/// next - ported verbatim from the TS `blankLine`.
fn blank_line(vm: Option<&VmState>, shot_failed: bool, bot_name: &str) -> String {
    let Some(vm) = vm else {
        return "Could not reach the server.".to_string();
    };
    if !vm.available {
        return "This server does not run per-bot machines.".to_string();
    }
    if vm.state == "none" {
        return format!("{bot_name} has no computer yet.");
    }
    if vm.state == "stopped" {
        return "Asleep. Its files and logins are kept.".to_string();
    }
    if shot_failed {
        return "Booting. The screen appears once the desktop is up.".to_string();
    }
    if vm.detail.is_empty() {
        "Starting…".to_string()
    } else {
        vm.detail.clone()
    }
}

/// The status-dot word. Ported verbatim from the TS `stateWord` - same
/// branch order as `blank_line` on purpose, so the two never disagree
/// about which world they are describing.
fn state_word(vm: Option<&VmState>, shot_failed: bool) -> &'static str {
    let Some(vm) = vm else {
        return "offline";
    };
    if !vm.available {
        return "unavailable";
    }
    if vm.state == "none" {
        return "none";
    }
    if vm.state == "stopped" {
        return "asleep";
    }
    if shot_failed {
        return "booting";
    }
    "live"
}

/// A `data:` URI for the `<img src>` that never leaves this app's own
/// process to make a second, unauthenticated request - see this module's
/// top doc on the desktop-401 trap.
fn to_data_uri(bytes: &[u8]) -> String {
    format!("data:image/png;base64,{}", base64_encode(bytes))
}

/// A plain base64 encoder (RFC 4648 §4, standard alphabet, `=` padding).
/// No new dependency for encoding one thumbnail-sized PNG per poll tick -
/// `reqwest`'s own dependency tree vendors a `base64` crate transitively,
/// but nothing in this workspace exposes it to `client` today, and adding
/// a direct dependency for one ~20-line, directly-testable function was
/// judged not worth it - see `## Results`'s judgment-call note.
///
/// `pub(crate)`: EXPORT-01 reuses this for the same reason it exists here.
/// `thread.rs`'s export download builds a `data:text/markdown` URI off the
/// same "authenticated fetch, hand the bytes to a declarative element"
/// shape `to_data_uri` above already established for the desktop-401 trap,
/// rather than a second hand-rolled encoder that would drift from this one
/// the first time either is touched.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// One-click markdown save after an authenticated export fetch (EXPORT-01 /
/// SEC5-05). No-ops when `window` is unavailable (native desktop).
pub(crate) fn download_markdown_file(filename: &str, bytes: &[u8]) {
    use wasm_bindgen::JsCast;
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Ok(element) = document.create_element("a") else {
        return;
    };
    let Ok(anchor) = element.dyn_into::<web_sys::HtmlAnchorElement>() else {
        return;
    };
    let href = format!(
        "data:text/markdown;charset=utf-8;base64,{}",
        base64_encode(bytes)
    );
    anchor.set_href(&href);
    anchor.set_download(filename);
    anchor.click();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn vm(available: bool, state: &str, detail: &str, view_path: Option<&str>) -> VmState {
        VmState {
            available,
            state: state.to_string(),
            detail: detail.to_string(),
            view_path: view_path.map(str::to_string),
        }
    }

    /* -------------------------------------------------- bite (a): every branch */

    #[test]
    fn blank_line_covers_every_branch() {
        assert_eq!(
            blank_line(None, false, "Dora"),
            "Could not reach the server."
        );
        assert_eq!(
            blank_line(Some(&vm(false, "unavailable", "", None)), false, "Dora"),
            "This server does not run per-bot machines."
        );
        assert_eq!(
            blank_line(Some(&vm(true, "none", "", None)), false, "Dora"),
            "Dora has no computer yet."
        );
        assert_eq!(
            blank_line(Some(&vm(true, "stopped", "", None)), false, "Dora"),
            "Asleep. Its files and logins are kept."
        );
        assert_eq!(
            blank_line(
                Some(&vm(true, "starting", "container booting", None)),
                true,
                "Dora"
            ),
            "Booting. The screen appears once the desktop is up."
        );
        assert_eq!(
            blank_line(Some(&vm(true, "starting", "", None)), false, "Dora"),
            "Starting…"
        );
        assert_eq!(
            blank_line(
                Some(&vm(true, "starting", "pulling image", None)),
                false,
                "Dora"
            ),
            "pulling image"
        );
    }

    #[test]
    fn state_word_covers_every_branch() {
        assert_eq!(state_word(None, false), "offline");
        assert_eq!(
            state_word(Some(&vm(false, "unavailable", "", None)), false),
            "unavailable"
        );
        assert_eq!(state_word(Some(&vm(true, "none", "", None)), false), "none");
        assert_eq!(
            state_word(Some(&vm(true, "stopped", "", None)), false),
            "asleep"
        );
        assert_eq!(
            state_word(Some(&vm(true, "starting", "", None)), true),
            "booting"
        );
        assert_eq!(
            state_word(Some(&vm(true, "running", "", None)), false),
            "live"
        );
    }

    /* --------------------------------------------------------------- bite (b) */

    /// The structural half of bite (b): `run_cycle` only ever calls the two
    /// fetchers it was handed - proven here by counting invocations of
    /// each, across a mount-shaped cycle (not live) and a live one. There
    /// is no third counter for "ensure calls" because `run_cycle` has no
    /// way to make one - see that function's own doc for why this is
    /// enforced by its signature, not by this test.
    #[tokio::test]
    async fn run_cycle_never_fetches_a_thumbnail_for_a_machine_that_is_not_live() {
        let state_calls = Cell::new(0u32);
        let thumbnail_calls = Cell::new(0u32);
        let (next, next_frame, shot) = run_cycle(
            0,
            || {
                state_calls.set(state_calls.get() + 1);
                async { Ok(vm(true, "none", "No machine yet.", None)) }
            },
            |_f| {
                thumbnail_calls.set(thumbnail_calls.get() + 1);
                async { Ok(Some(vec![])) }
            },
        )
        .await;

        assert_eq!(state_calls.get(), 1, "a cycle always reads state once");
        assert_eq!(
            thumbnail_calls.get(),
            0,
            "bite (c): a machine that is not live must not cost a thumbnail fetch"
        );
        assert_eq!(next.map(|v| v.state), Some("none".to_string()));
        assert_eq!(
            next_frame, 0,
            "the frame counter must not move for a machine that is not live"
        );
        assert!(shot.is_none());
    }

    #[tokio::test]
    async fn run_cycle_fetches_a_thumbnail_and_bumps_the_frame_for_a_live_machine() {
        let thumbnail_calls = Cell::new(0u32);
        let (next, next_frame, shot) = run_cycle(
            4,
            || async { Ok(vm(true, "running", "", None)) },
            |f| {
                thumbnail_calls.set(thumbnail_calls.get() + 1);
                assert_eq!(f, 5, "the thumbnail fetch must use the just-bumped frame");
                async { Ok(Some(vec![1, 2, 3])) }
            },
        )
        .await;

        assert_eq!(thumbnail_calls.get(), 1);
        assert_eq!(next.map(|v| v.state), Some("running".to_string()));
        assert_eq!(next_frame, 5);
        assert_eq!(shot, Some(Ok(Some(vec![1, 2, 3]))));
    }

    /// A failed state read is treated as "nothing" - ported from the TS
    /// `load`'s own `catch { setVm(null) }`, not "keep showing the last
    /// answer" (which would be a stale picture of a machine that might no
    /// longer exist at all).
    #[tokio::test]
    async fn run_cycle_treats_a_failed_state_read_as_no_machine() {
        let (next, _frame, shot) = run_cycle(
            0,
            || async { Err::<VmState, String>("network error".to_string()) },
            |_f| async { Ok(Some(vec![])) },
        )
        .await;
        assert!(next.is_none());
        assert!(shot.is_none());
    }

    /* --------------------------------------------------------------- bite (c) */

    /// **Bite (c), on the pure function directly.** Guard present:
    /// `next_frame` only increments when `live` is true.
    ///
    /// **Mutation run (captured for `## Results`, then reverted by
    /// re-editing - never `git checkout`):** changed the body to
    /// `current + 1` unconditionally. `next_frame_holds_steady_when_not_live`
    /// went red (`0 != 1`); `next_frame_advances_when_live` stayed green
    /// (it already expected `+1`) - exactly the "specific case" the ticket
    /// names, not a wholesale failure.
    #[test]
    fn next_frame_holds_steady_when_not_live() {
        assert_eq!(next_frame(7, false), 7);
    }

    #[test]
    fn next_frame_advances_when_live() {
        assert_eq!(next_frame(7, true), 8);
    }

    /* ------------------------------------------------------- base64 encoding */

    #[test]
    fn base64_encode_matches_rfc_4648_test_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn to_data_uri_wraps_base64_png_bytes() {
        assert_eq!(to_data_uri(b"foo"), "data:image/png;base64,Zm9v");
    }
}
