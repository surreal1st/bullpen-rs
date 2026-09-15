//! S13b-02: the pure half of window-geometry persistence. `desktop.rs` owns
//! wiring this into the live window (event capture, monitor queries, tao
//! types); this module stays free of `dioxus`/`tao` so [`usable`] - "the
//! whole reason this ticket has tests" (its own ticket's words) - can be
//! unit tested without a running window, the same shape S13b-01's
//! `transport::native` split pure logic from the async/network half.
//!
//! **Units.** Every field here is a physical pixel, matching `tao`'s own
//! `PhysicalPosition`/`PhysicalSize` - `desktop.rs` reads `Moved`/`Resized`
//! event payloads (already physical) straight into [`WindowState`] and
//! passes [`WindowState`] straight back into `set_outer_position`/
//! `set_inner_size` (also physical), so geometry never crosses a
//! logical/physical conversion and can never drift with a monitor's DPI
//! scale factor.
//!
//! **File location.** `%USERPROFILE%\.bullpen\window.json`, beside the
//! silent sign-in's own `password.key`
//! (`transport::native::password_key_path`) - the orchestrator's ticket
//! decision, not `%APPDATA%`: this app's per-user state already lives in
//! `.bullpen`. `BULLPEN_WINDOW_STATE_FILE` overrides it, mirroring
//! `BULLPEN_PASSWORD_FILE`, purely so a test can point this at a
//! disposable file instead of Josh's real one.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A saved window geometry - outer (frame) position, inner (client area)
/// size. See this module's top doc for why both are physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
}

/// A monitor's rect, in the same physical-pixel space as [`WindowState`].
/// A plain struct - not `tao::monitor::MonitorHandle`, which nothing can
/// construct outside a running window - so [`usable`] stays pure and
/// testable. `desktop.rs` builds these from `Window::available_monitors`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Minimum strip of a saved window that must land on some monitor for a
/// restore to count as safe. Sized to the title bar: wide enough to grab
/// with the mouse and drag the rest of the window back into view, tall
/// enough to be the title bar rather than a sliver of client area a click
/// would just pass through. Bite (b) is exactly this: a corner overlap
/// under this threshold must still restore to `None`.
const MIN_VISIBLE_WIDTH: f64 = 160.0;
const MIN_VISIBLE_HEIGHT: f64 = 40.0;

/// The restore guard - ticket's own words: "the whole reason this ticket
/// has tests". `None` means "do not trust the saved geometry"; the caller
/// (`desktop.rs`) falls back to `DEFAULT_WIDTH`/`DEFAULT_HEIGHT` centred on
/// a monitor that still exists. A saved rect is usable only if it
/// overlaps *some* monitor by at least `MIN_VISIBLE_WIDTH` x
/// `MIN_VISIBLE_HEIGHT` - a window last closed on a monitor that is now
/// unplugged, or barely clipping a corner of one that remains, never
/// restores somewhere Josh cannot reach it with the mouse.
pub fn usable(state: &WindowState, monitors: &[Rect]) -> Option<WindowState> {
    let fits_some_monitor = monitors.iter().any(|monitor| {
        let overlap_width = overlap(state.x, state.width, monitor.x, monitor.width);
        let overlap_height = overlap(state.y, state.height, monitor.y, monitor.height);
        overlap_width >= MIN_VISIBLE_WIDTH && overlap_height >= MIN_VISIBLE_HEIGHT
    });
    fits_some_monitor.then_some(*state)
}

/// 1-D overlap length between `[a, a + a_len)` and `[b, b + b_len)`,
/// clamped to 0 when the intervals do not overlap at all (rather than
/// going negative, which would let two near-misses cancel out into a
/// false-positive `usable` on the other axis).
fn overlap(a: f64, a_len: f64, b: f64, b_len: f64) -> f64 {
    let start = a.max(b);
    let end = (a + a_len).min(b + b_len);
    (end - start).max(0.0)
}

/// Where saved window geometry lives - see this module's top doc.
pub fn window_state_file_path() -> Option<PathBuf> {
    if let Ok(overridden) = std::env::var("BULLPEN_WINDOW_STATE_FILE") {
        return Some(PathBuf::from(overridden));
    }
    std::env::var("USERPROFILE")
        .ok()
        .map(|home| Path::new(&home).join(".bullpen").join("window.json"))
}

/// Read a previously saved [`WindowState`] from `path`. Any failure - file
/// missing, unreadable, or JSON that doesn't parse (bite (c): a
/// garbage/truncated `window.json`) - returns `None` rather than
/// panicking; the caller already treats `None` the same as "no saved
/// state, use the centred default".
pub fn load(path: &Path) -> Option<WindowState> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Write `state` to `path` as JSON, creating the parent directory if it
/// does not exist yet (`.bullpen` already exists in practice once the
/// silent sign-in's own key file is there, but a fresh test temp dir has
/// no such guarantee). Errors are swallowed on purpose: rule 2's own text
/// ("a hard kill losing the last geometry is acceptable") means a failed
/// write here is never worth a panic or a dialog Josh did not ask for, the
/// same posture `desktop.rs`'s silent sign-in already takes on its own
/// failures.
pub fn save(path: &Path, state: &WindowState) {
    let Ok(json) = serde_json::to_string(state) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json);
}

#[cfg(test)]
#[path = "window_state_tests.rs"]
mod window_state_tests;
