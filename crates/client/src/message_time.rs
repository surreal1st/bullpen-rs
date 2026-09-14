//! Port of `projects/bullpen-night/src/shared/messageTime.ts`: when a
//! message was sent, in the READER'S local time. Messages are stored as UTC
//! ISO strings; formatting the stored string directly (e.g. slicing the
//! first 10 characters for a day key) puts anything after 8pm Eastern on the
//! following calendar day, which is why every function here goes through a
//! real `Date` rather than string-slicing.
//!
//! Built on `js_sys::Date` rather than `chrono`: `toLocaleTimeString` /
//! `toLocaleDateString` are exactly the locale-aware weekday/month
//! formatting the original leans on, and re-deriving that from scratch in
//! `chrono` (no ICU here) would be a second, worse implementation of the
//! same thing.
//!
//! 🔴 Deviation: the TS calls these with `locale: undefined`, meaning "the
//! browser's own default". `js_sys::Date`'s stable (non-`js_sys_unstable_apis`)
//! bindings type that argument as a plain `&str`, which cannot express
//! `undefined` - so this pins `"en-US"` rather than reading the browser's
//! locale. Fine for Josh; wrong for a non-US reader, and worth fixing if
//! this ever ships to one.
//!
//! Only meaningfully runs in a browser (like the rest of `client`, e.g.
//! `app.rs`'s `gloo-net` calls) - `js_sys::Date` compiles on any target but
//! is not something `cargo test` can drive, which is why the bite check for
//! this ticket lives in `api::feed` instead, where it can.

use js_sys::{Date, Object, Reflect};
use wasm_bindgen::JsValue;

const LOCALE: &str = "en-US";
const DAY_MS: f64 = 86_400_000.0;

fn parse(iso: &str) -> Option<Date> {
    let date = Date::new(&JsValue::from_str(iso));
    if date.get_time().is_nan() {
        None
    } else {
        Some(date)
    }
}

/// The clock time on a message row, e.g. "11:25 PM".
pub fn format_time(iso: &str) -> String {
    let Some(date) = parse(iso) else {
        return String::new();
    };
    let opts = Object::new();
    let _ = Reflect::set(&opts, &"hour".into(), &"numeric".into());
    let _ = Reflect::set(&opts, &"minute".into(), &"2-digit".into());
    date.to_locale_time_string_with_options(LOCALE, &opts)
        .into()
}

/// A stable key for the LOCAL calendar day, built from the local
/// year/month/date getters rather than `iso[..10]` - see the module doc.
pub fn day_key(iso: &str) -> String {
    let Some(date) = parse(iso) else {
        return String::new();
    };
    format!(
        "{:04}-{:02}-{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date()
    )
}

/// The divider label: "Today", "Yesterday", or a written date. `now` is a
/// parameter rather than read from the clock so the boundary stays testable
/// in principle, matching the TS signature - though see the module doc on
/// why `cargo test` cannot exercise this one either.
pub fn format_day(iso: &str, now: &Date) -> String {
    let Some(date) = parse(iso) else {
        return String::new();
    };
    let now_iso: String = now.to_iso_string().as_string().unwrap_or_default();
    let key = day_key(iso);
    if key == day_key(&now_iso) {
        return "Today".to_string();
    }

    let yesterday = Date::new(&JsValue::from_f64(now.get_time() - DAY_MS));
    let yesterday_iso: String = yesterday.to_iso_string().as_string().unwrap_or_default();
    if key == day_key(&yesterday_iso) {
        return "Yesterday".to_string();
    }

    // Inside the last week, the weekday is what someone actually remembers.
    let days = ((start_of_day(now).get_time() - start_of_day(&date).get_time()) / DAY_MS).round();
    if days > 0.0 && days < 7.0 {
        let opts = Object::new();
        let _ = Reflect::set(&opts, &"weekday".into(), &"long".into());
        return date.to_locale_date_string(LOCALE, &opts).into();
    }

    // The year only when it is not this one.
    let opts = Object::new();
    let _ = Reflect::set(&opts, &"weekday".into(), &"short".into());
    let _ = Reflect::set(&opts, &"month".into(), &"long".into());
    let _ = Reflect::set(&opts, &"day".into(), &"numeric".into());
    if date.get_full_year() != now.get_full_year() {
        let _ = Reflect::set(&opts, &"year".into(), &"numeric".into());
    }
    date.to_locale_date_string(LOCALE, &opts).into()
}

fn start_of_day(date: &Date) -> Date {
    let copy = Date::new(&JsValue::from_f64(date.get_time()));
    copy.set_hours(0);
    copy.set_minutes(0);
    copy.set_seconds(0);
    copy.set_milliseconds(0);
    copy
}

/// The full thing, for a tooltip on the short time.
pub fn format_full(iso: &str) -> String {
    let Some(date) = parse(iso) else {
        return String::new();
    };
    let opts = Object::new();
    let _ = Reflect::set(&opts, &"weekday".into(), &"long".into());
    let _ = Reflect::set(&opts, &"year".into(), &"numeric".into());
    let _ = Reflect::set(&opts, &"month".into(), &"long".into());
    let _ = Reflect::set(&opts, &"day".into(), &"numeric".into());
    let _ = Reflect::set(&opts, &"hour".into(), &"numeric".into());
    let _ = Reflect::set(&opts, &"minute".into(), &"2-digit".into());
    date.to_locale_string(LOCALE, &opts).into()
}

/// Now, as an ISO string - used for the live streaming row's timestamp
/// (there is no `createdAt` from the server yet; the row is local until the
/// run finishes).
pub fn now_iso() -> String {
    Date::new_0()
        .to_iso_string()
        .as_string()
        .unwrap_or_default()
}
