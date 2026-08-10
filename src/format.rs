//! Shared time-formatting helpers for rendering Unix timestamps.
//!
//! Backed by [`glib::DateTime`] for locale- and timezone-aware formatting, so
//! there is no external date dependency. Two styles are provided: a compact
//! native-Telegram sidebar label ([`sidebar_time`]) and a plain 24-hour clock
//! ([`message_time`]).

use gtk::glib;

/// Format a Unix timestamp (seconds) as a native-Telegram sidebar label.
///
/// The result adapts to how recent the timestamp is, in the local timezone:
/// - today → `"HH:MM"` (24-hour, e.g. `"14:30"`)
/// - yesterday → `"Yesterday"`
/// - within the last 7 days → the short weekday name (e.g. `"Mon"`)
/// - otherwise → `"DD/MM/YY"`
///
/// Returns an empty string if the timestamp or the current time can't be
/// resolved.
pub fn sidebar_time(unix_secs: i64) -> String {
    let Ok(dt) = glib::DateTime::from_unix_local(unix_secs) else {
        return String::new();
    };
    let Ok(now) = glib::DateTime::now_local() else {
        return String::new();
    };

    // Today: same local calendar date.
    if same_date(&dt, &now) {
        return dt
            .format("%H:%M")
            .map(|g| g.to_string())
            .unwrap_or_default();
    }

    // Yesterday: matches now shifted back one day.
    if let Ok(yesterday) = now.add_days(-1) {
        if same_date(&dt, &yesterday) {
            return "Yesterday".to_string();
        }
    }

    // Within the last week (2..=6 days ago): short weekday name.
    for i in 2..=6 {
        if let Ok(day) = now.add_days(-i) {
            if same_date(&dt, &day) {
                return dt.format("%a").map(|g| g.to_string()).unwrap_or_default();
            }
        }
    }

    // Older: absolute date.
    dt.format("%d/%m/%y")
        .map(|g| g.to_string())
        .unwrap_or_default()
}

/// Format a Unix timestamp (seconds) as a plain 24-hour `"HH:MM"` clock in the
/// local timezone. Returns an empty string if the timestamp can't be resolved.
#[allow(dead_code)]
pub fn message_time(unix_secs: i64) -> String {
    glib::DateTime::from_unix_local(unix_secs)
        .and_then(|d| d.format("%H:%M"))
        .map(|g| g.to_string())
        .unwrap_or_default()
}

/// Whether two [`glib::DateTime`] values fall on the same local calendar date.
fn same_date(a: &glib::DateTime, b: &glib::DateTime) -> bool {
    a.year() == b.year() && a.month() == b.month() && a.day_of_month() == b.day_of_month()
}
