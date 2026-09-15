//! Port of `projects/bullpen-night/src/shared/messageTime.ts`: when a
//! message was sent, in the READER'S local time. Messages are stored as UTC
//! ISO strings; formatting the stored string directly (e.g. slicing the
//! first 10 characters for a day key) puts anything after 8pm Eastern on the
//! following calendar day, which is why every function here goes through a
//! real parsed timestamp rather than string-slicing.
//!
//! S13a-01b: this used to be one `js_sys::Date`-only implementation and, per
//! its own doc, "only meaningfully runs in a browser". It is now a seam like
//! `transport/`'s - `web.rs` (`#[cfg(target_arch = "wasm32")]`) is that
//! original implementation, untouched; `native.rs`
//! (`#[cfg(not(target_arch = "wasm32"))]`) is a fresh `chrono` one for the
//! desktop build, which has no browser to ask.
//!
//! 🔴 **Desktop time formatting is not locale-driven.** `web.rs` leans on
//! `toLocaleTimeString`/`toLocaleDateString` for the browser's own locale
//! (pinned to `"en-US"` there already - see that module's own doc on why).
//! `native.rs` has no such thing to call - `chrono` alone carries no ICU/CLDR
//! locale data, so re-deriving real locale-aware formatting from scratch
//! would be a second, worse implementation of the same thing. The desktop
//! build therefore always renders a fixed `en-US`-shaped format (`format_time`:
//! "7:05 PM"; `format_day`: "Today" / "Yesterday" / "Monday" / "Mon January
//! 5" / "Mon January 5, 2025") regardless of Josh's OS locale. This is a
//! visible behaviour change from the web build, not an oversight - worth
//! revisiting if bullpen-rs ever ships desktop to a non-US reader.
//!
//! Both halves resolve a stored UTC ISO string into the READER's wall clock
//! by going through the platform's own notion of the local timezone
//! (`Date`'s local getters on web; `chrono::Local` on native, which uses the
//! OS timezone database) - never a hardcoded offset.

use chrono::{DateTime, Utc};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
pub use native::{day_key, format_day, format_full, format_time};
#[cfg(target_arch = "wasm32")]
pub use web::{day_key, format_day, format_full, format_time};

/// Now, as a UTC ISO string with millisecond precision - used for the live
/// streaming row's timestamp (there is no `createdAt` from the server yet;
/// the row is local until the run finishes) and as `format_day`'s `now`
/// argument (`thread.rs`). Millisecond precision, `Z` suffix: the same shape
/// `crates/server/src/routes/mod.rs::now_iso` and
/// `web.rs`'s `Date::to_iso_string` both already produce, so every
/// `created_at` this client ever sees or makes parses the same way
/// regardless of which one wrote it.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Milliseconds since the Unix epoch, or `None` for a string that does not
/// parse - `thread.rs::paused`'s "15+ minutes since the last message" check,
/// which only ever compares two of these against each other and so has no
/// locale dependency to split on. `chrono::DateTime::parse_from_rfc3339` on
/// both platforms (unlike the formatting functions above, this needs no
/// browser/OS timezone lookup either - a millisecond-since-epoch diff is the
/// same number in every timezone).
pub fn parse_epoch_ms(iso: &str) -> Option<f64> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|d| d.timestamp_millis() as f64)
}
