//! S13b-02's four named bites for [`super::usable`], plus [`super::load`]/
//! [`super::save`] - see those functions' own docs, and `desktop.rs` for
//! how this plugs into the real desktop startup path. A new file (rather
//! than folding into `window_state.rs`'s own inline module), matching
//! S13b-01's "Owns: ... and a new test file" shape.
//!
//! Nested into `window_state.rs` via `#[path = "window_state_tests.rs"]
//! mod window_state_tests;`, so this is a child of `window_state`'s own
//! module scope - `use super::*` reaches every private item the same way
//! `native.rs`'s own test module already does.
//!
//! Bites (c) and (d) never touch Josh's real
//! `%USERPROFILE%\.bullpen\window.json` - each writes its own throwaway
//! file under the OS temp directory and points `load`/`save` at that
//! file's path directly.

use super::*;
use std::path::PathBuf;

/// A throwaway file path under the OS temp root, unique per call so
/// parallel tests never collide - same reasoning as
/// `transport::native::silent_sign_in_tests::unique_temp_dir`.
fn unique_temp_file(label: &str) -> PathBuf {
    let unique = format!(
        "bullpen-rs-window-state-{label}-{pid}-{nanos}.json",
        pid = std::process::id(),
        nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

/// A single 1920x1080 monitor at the origin - the common case every bite
/// below reasons against.
fn one_monitor() -> Vec<Rect> {
    vec![Rect {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    }]
}

/// Bite (a): a saved rect that lies wholly outside every monitor rect
/// returns `None`.
#[test]
fn wholly_offscreen_rect_is_not_usable() {
    let state = WindowState {
        width: 1280.0,
        height: 860.0,
        x: 5000.0,
        y: 5000.0,
    };
    assert_eq!(usable(&state, &one_monitor()), None);
}

/// Bite (b): a rect overlapping a monitor by only a few pixels of corner -
/// well under the title-bar-sized minimum this module requires - is still
/// `None`. Only a 5x5 pixel square of this window lands on the monitor
/// (bottom-right corner), far short of `MIN_VISIBLE_WIDTH` x
/// `MIN_VISIBLE_HEIGHT`.
#[test]
fn sliver_corner_overlap_is_not_usable() {
    let state = WindowState {
        width: 200.0,
        height: 200.0,
        x: 1915.0,
        y: 1075.0,
    };
    assert_eq!(usable(&state, &one_monitor()), None);
}

/// A sanity companion to (a)/(b), not one of the ticket's four named
/// bites: a rect that genuinely overlaps a monitor by more than the
/// minimum IS usable. Without this, a guard hard-coded to always return
/// `None` would still pass every test above.
#[test]
fn comfortably_onscreen_rect_is_usable() {
    let state = WindowState {
        width: 1280.0,
        height: 860.0,
        x: 100.0,
        y: 100.0,
    };
    assert_eq!(usable(&state, &one_monitor()), Some(state));
}

/// Bite (c): a garbage/truncated `window.json` restores defaults (`None`)
/// rather than panicking.
#[test]
fn truncated_json_restores_defaults_without_panicking() {
    let path = unique_temp_file("truncated");
    std::fs::write(&path, "{\"width\":").expect("temp dir is writable");
    assert_eq!(load(&path), None);
    let _ = std::fs::remove_file(&path);
}

/// Bite (d): a state written then read back is identical - includes a
/// negative `x` (a monitor to the left of the primary places windows at
/// negative coordinates), so a stray unsigned cast anywhere in the path
/// would show up here.
#[test]
fn round_trip_write_then_read_is_identical() {
    let path = unique_temp_file("roundtrip");
    let state = WindowState {
        width: 1400.5,
        height: 900.25,
        x: -120.0,
        y: 30.0,
    };
    save(&path, &state);
    assert_eq!(load(&path), Some(state));
    let _ = std::fs::remove_file(&path);
}
