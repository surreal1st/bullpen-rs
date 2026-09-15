//! Schedules, written the way a person says them.
//!
//! Deliberately not cron. Humans read and write these, and `43 8 * * 1-5` is a
//! thing you have to decode rather than read. Four shapes cover everything the
//! old roster actually did:
//!
//!   every 15 minutes
//!   hourly
//!   daily at 07:30
//!   weekdays at 08:43
//!
//! Times use the local time, because "08:43" means breakfast, not UTC.
//! `next_run` takes and returns `DateTime<Utc>` but performs comparisons and
//! increments using local time, the same way the TS code works.

use chrono::{DateTime, Datelike, Local, Timelike, Utc, Weekday};

/// Represents a schedule in human-readable form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// Every N minutes, where N >= 5.
    Interval {
        minutes: u32,
        from_hour: Option<u32>,
        to_hour: Option<u32>,
    },
    /// Every hour on a specific minute.
    Hourly { minute: u32 },
    /// Once per day at a specific time.
    Daily { hour: u32, minute: u32 },
    /// On weekdays (Mon-Fri) at a specific time.
    Weekdays { hour: u32, minute: u32 },
    /// On specific days at specific times.
    Clock {
        days: Vec<u32>,
        times: Vec<(u32, u32)>,
    },
}

const EXAMPLES: &str = "every 15 minutes, hourly, daily at 07:30, weekdays at 08:43";

const DAY_NAMES: &[&str] = &[
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// Parses a human-readable schedule string into a Schedule.
pub fn parse_schedule(input: &str) -> Result<Schedule, String> {
    let text = input.trim().to_lowercase();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");

    if text.is_empty() {
        return Err(format!("Give a schedule, such as: {}", EXAMPLES));
    }

    if text == "hourly" || text == "every hour" {
        return Ok(Schedule::Hourly { minute: 0 });
    }

    // Try interval: "every N minutes" or "every N hours"
    if let Some(rest) = text.strip_prefix("every ") {
        let parts: Vec<&str> = rest.split(' ').collect();
        if parts.len() == 2
            && let Ok(n) = parts[0].parse::<u32>()
        {
            let unit = parts[1];
            if matches!(unit, "minute" | "minutes" | "min" | "hour" | "hours") {
                let minutes = if unit.starts_with("hour") { n * 60 } else { n };

                if minutes < 5 {
                    return Err("The shortest interval is 5 minutes.".to_string());
                }
                if minutes > 60 * 24 * 7 {
                    return Err("That is longer than a week.".to_string());
                }
                return Ok(Schedule::Interval {
                    minutes,
                    from_hour: None,
                    to_hour: None,
                });
            }
        }
    }

    // Try "daily at HH:MM" or "weekdays at HH:MM"
    if text.contains(" at ") {
        let at_idx = text.find(" at ").unwrap();
        let kind_part = &text[..at_idx];
        let time_part = &text[at_idx + 4..];

        if let Some((hour, minute)) = parse_time(time_part) {
            if hour > 23 || minute > 59 {
                return Err("That is not a time of day.".to_string());
            }

            if kind_part == "daily" || kind_part == "every day" {
                return Ok(Schedule::Daily { hour, minute });
            } else if kind_part == "weekdays" || kind_part == "weekday" {
                return Ok(Schedule::Weekdays { hour, minute });
            } else if let Ok(Some(sched)) = parse_clock(&text) {
                return Ok(sched);
            }
        } else if let Ok(Some(sched)) = parse_clock(&text) {
            return Ok(sched);
        }
    }

    Err(format!(
        "That is not a schedule I understand. Try: {}",
        EXAMPLES
    ))
}

/// Parse a single time "HH:MM"
fn parse_time(text: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hour = parts[0].parse::<u32>().ok()?;
    let minute = parts[1].parse::<u32>().ok()?;
    Some((hour, minute))
}

/// Parses "mondays at 09:00", "tuesdays and thursdays at 11:32", etc.
fn parse_clock(text: &str) -> Result<Option<Schedule>, String> {
    let at_idx = match text.rfind(" at ") {
        Some(idx) => idx,
        None => return Ok(None),
    };

    let day_part = text[..at_idx].trim();
    let time_part = text[at_idx + 4..].trim();

    // Check that time_part looks reasonable (digits, colons, commas, "and", spaces)
    let time_chars_ok = time_part.chars().all(|c| {
        c.is_ascii_digit() || c == ':' || c == ',' || c == ' ' || c == 'a' || c == 'n' || c == 'd'
    });
    if !time_chars_ok {
        return Ok(None);
    }

    let days = parse_days(day_part)?;
    let Some(days) = days else {
        return Ok(None);
    };

    // Parse times from time_part, handling "09:00, 13:00 and 17:00" format
    let mut times: Vec<(u32, u32)> = Vec::new();

    // Replace " and " with commas for easier splitting
    let normalized = time_part.replace(" and ", ",");

    for chunk in normalized.split(',') {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some((hour, minute)) = parse_time(trimmed) {
            if hour > 23 || minute > 59 {
                return Err("That is not a time of day.".to_string());
            }
            times.push((hour, minute));
        } else {
            return Err(format!("`{}` is not a time of day.", trimmed));
        }
    }

    // Sort and dedupe times
    times.sort();
    times.dedup();

    if times.is_empty() {
        return Ok(None);
    }

    times.sort_by_key(|(h, m)| (*h, *m));
    Ok(Some(Schedule::Clock { days, times }))
}

/// Parses day names like "monday", "weekdays", etc.
fn parse_days(text: &str) -> Result<Option<Vec<u32>>, String> {
    if text == "daily" || text == "every day" {
        return Ok(Some(vec![0, 1, 2, 3, 4, 5, 6]));
    }
    if text == "weekdays" || text == "weekday" {
        return Ok(Some(vec![1, 2, 3, 4, 5]));
    }
    if text == "weekends" || text == "weekend" {
        return Ok(Some(vec![0, 6]));
    }

    let mut found: Vec<u32> = Vec::new();

    for word in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let w = word.trim().trim_end_matches('s');
        if w.is_empty() || w == "and" {
            continue;
        }

        if let Some(idx) = DAY_NAMES.iter().position(|&d| d == w) {
            if !found.contains(&(idx as u32)) {
                found.push(idx as u32);
            }
        } else {
            return Ok(None);
        }
    }

    if found.is_empty() {
        return Ok(None);
    }

    found.sort();
    Ok(Some(found))
}

/// Describes a schedule in human-readable form.
pub fn describe_schedule(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Interval {
            minutes,
            from_hour: _,
            to_hour: _,
        } => {
            if minutes % 60 == 0 {
                if *minutes == 60 {
                    "every hour".to_string()
                } else {
                    format!("every {} hours", minutes / 60)
                }
            } else {
                format!("every {} minutes", minutes)
            }
        }
        Schedule::Hourly { minute: _ } => "every hour".to_string(),
        Schedule::Daily { hour, minute } => {
            format!("daily at {:02}:{:02}", hour, minute)
        }
        Schedule::Weekdays { hour, minute } => {
            format!("weekdays at {:02}:{:02}", hour, minute)
        }
        Schedule::Clock { days, times } => {
            let days_str = list_days(days);
            let times_str = list_times(times);
            format!("{} at {}", days_str, times_str)
        }
    }
}

/// Lists days in human-readable form.
fn list_days(days: &[u32]) -> String {
    if days.len() == 7 {
        return "daily".to_string();
    }
    if days.len() == 5 && days.iter().all(|d| (1..=5).contains(d)) {
        return "weekdays".to_string();
    }
    if days.len() == 2 && days.contains(&0) && days.contains(&6) {
        return "weekends".to_string();
    }

    let names: Vec<String> = days
        .iter()
        .filter_map(|&d| DAY_NAMES.get(d as usize).map(|n| format!("{}s", n)))
        .collect();

    if names.is_empty() {
        "".to_string()
    } else if names.len() == 1 {
        names[0].clone()
    } else {
        let mut result = names[..names.len() - 1].join(", ");
        result.push_str(" and ");
        result.push_str(&names[names.len() - 1]);
        result
    }
}

/// Lists times in human-readable form.
fn list_times(times: &[(u32, u32)]) -> String {
    let parts: Vec<String> = times
        .iter()
        .map(|(h, m)| format!("{:02}:{:02}", h, m))
        .collect();

    if parts.is_empty() {
        "".to_string()
    } else if parts.len() == 1 {
        parts[0].clone()
    } else {
        let mut result = parts[..parts.len() - 1].join(", ");
        result.push_str(" and ");
        result.push_str(&parts[parts.len() - 1]);
        result
    }
}

/// Returns true if the hour is within the window (handles wrapping past midnight).
fn in_window(hour: u32, from_hour: u32, to_hour: u32) -> bool {
    if from_hour <= to_hour {
        hour >= from_hour && hour <= to_hour
    } else {
        hour >= from_hour || hour <= to_hour
    }
}

/// Calculates the next time this schedule should fire, strictly after `from`.
pub fn next_run(schedule: &Schedule, from: DateTime<Utc>) -> DateTime<Utc> {
    let from_local = from.with_timezone(&Local);

    let next_local = match schedule {
        Schedule::Interval {
            minutes,
            from_hour,
            to_hour,
        } => {
            let mut next = from_local + chrono::Duration::minutes(*minutes as i64);

            if let (Some(from_h), Some(to_h)) = (from_hour, to_hour) {
                let mut guard = 0;
                while !in_window(next.hour(), *from_h, *to_h) && guard < 48 {
                    next = next.with_hour((next.hour() + 1) % 24).unwrap_or(next);
                    next = next
                        .with_minute(0)
                        .unwrap_or(next)
                        .with_second(0)
                        .unwrap_or(next);
                    guard += 1;
                }
            }
            next
        }
        Schedule::Clock { days, times } => {
            for ahead in 0..=8 {
                let day = from_local + chrono::Duration::days(ahead as i64);
                let day_of_week = day.weekday().num_days_from_sunday();
                if !days.contains(&day_of_week) {
                    continue;
                }

                for &(hour, minute) in times {
                    let mut test_time = day.with_hour(hour).unwrap_or(day);
                    test_time = test_time
                        .with_minute(minute)
                        .unwrap_or(test_time)
                        .with_second(0)
                        .unwrap_or(test_time);

                    if test_time > from_local {
                        return test_time.with_timezone(&Utc);
                    }
                }
            }
            from_local + chrono::Duration::days(7)
        }
        Schedule::Hourly { minute } => {
            let mut next = from_local.with_minute(*minute).unwrap_or(from_local);
            next = next.with_second(0).unwrap_or(next);
            if next <= from_local {
                next += chrono::Duration::hours(1);
            }
            next
        }
        Schedule::Daily { hour, minute } => {
            let mut next = from_local.with_hour(*hour).unwrap_or(from_local);
            next = next
                .with_minute(*minute)
                .unwrap_or(next)
                .with_second(0)
                .unwrap_or(next);
            if next <= from_local {
                next += chrono::Duration::days(1);
            }
            next
        }
        Schedule::Weekdays { hour, minute } => {
            let mut next = from_local.with_hour(*hour).unwrap_or(from_local);
            next = next
                .with_minute(*minute)
                .unwrap_or(next)
                .with_second(0)
                .unwrap_or(next);
            if next <= from_local {
                next += chrono::Duration::days(1);
            }

            while next.weekday() == Weekday::Sun || next.weekday() == Weekday::Sat {
                next += chrono::Duration::days(1);
            }
            next
        }
    };

    next_local.with_timezone(&Utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_schedule_understands_four_shapes() {
        let result = parse_schedule("every 15 minutes");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Interval { minutes, .. } => assert_eq!(minutes, 15),
            _ => panic!("Expected interval"),
        }

        let result = parse_schedule("every 2 hours");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Interval { minutes, .. } => assert_eq!(minutes, 120),
            _ => panic!("Expected interval"),
        }

        let result = parse_schedule("hourly");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Hourly { .. } => {}
            _ => panic!("Expected hourly"),
        }

        let result = parse_schedule("daily at 07:30");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Daily { hour, minute } => {
                assert_eq!(hour, 7);
                assert_eq!(minute, 30);
            }
            _ => panic!("Expected daily"),
        }

        let result = parse_schedule("weekdays at 08:43");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Weekdays { hour, minute } => {
                assert_eq!(hour, 8);
                assert_eq!(minute, 43);
            }
            _ => panic!("Expected weekdays"),
        }
    }

    #[test]
    fn test_parse_schedule_rejects_unknown_format() {
        let result = parse_schedule("43 8 * * 1-5");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("daily at 07:30"));
    }

    #[test]
    fn test_parse_schedule_enforces_interval_floor() {
        let result = parse_schedule("every 1 minutes");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "The shortest interval is 5 minutes.");

        let result = parse_schedule("every 5 minutes");
        assert!(result.is_ok());
    }

    #[test]
    fn test_describe_schedule_roundtrips() {
        for text in &[
            "every 15 minutes",
            "every 2 hours",
            "daily at 07:30",
            "weekdays at 08:43",
        ] {
            let parsed = parse_schedule(text).expect("parse");
            let described = describe_schedule(&parsed);
            assert_eq!(&described, text, "failed for: {}", text);
        }
    }

    #[test]
    fn test_next_run_is_always_in_future() {
        let from = DateTime::<Utc>::from_naive_utc_and_offset(
            DateTime::from_timestamp(1725955800, 0).unwrap().naive_utc(),
            Utc,
        );

        for text in &[
            "daily at 07:30",
            "hourly",
            "every 15 minutes",
            "weekdays at 08:43",
        ] {
            let s = parse_schedule(text).expect("parse");
            let next = next_run(&s, from);
            assert!(next > from, "next run not in future for: {}", text);
        }
    }

    #[test]
    fn test_weekdays_skip_weekend() {
        // Friday 2026-09-11 at 09:00 LOCAL, past that day's 08:43 - built in
        // Local so the weekday is the weekday the schedule sees. (The first
        // version hand-wrote a UTC epoch that was a Wednesday in 2024.)
        use chrono::TimeZone;
        let friday_utc = Local
            .with_ymd_and_hms(2026, 9, 11, 9, 0, 0)
            .single()
            .expect("unambiguous local time")
            .with_timezone(&Utc);
        assert_eq!(friday_utc.with_timezone(&Local).weekday(), Weekday::Fri);
        let s = parse_schedule("weekdays at 08:43").expect("parse");
        let next = next_run(&s, friday_utc);
        let next_local = next.with_timezone(&Local);
        assert_eq!(next_local.weekday(), Weekday::Mon, "should skip to Monday");
    }
}
