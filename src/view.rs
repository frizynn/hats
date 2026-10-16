//! Text shared by `hats list` and the interactive view.

use std::time::SystemTime;

use crate::limits::{LimitWindow, unix_secs};

pub fn window_short(name: &str) -> &str {
    match name {
        "five_hour" => "5h",
        "seven_day" => "7d",
        other => other,
    }
}

/// `▰▰▰▱▱▱▱▱▱▱` for 30 % in 10 cells.
pub fn bar(percent: f64, cells: usize) -> String {
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    format!("{}{}", "▰".repeat(filled), "▱".repeat(cells - filled))
}

/// Used percent now: a window that already reset counts as empty.
pub fn used_now(w: &LimitWindow, now: SystemTime) -> f64 {
    if w.resets_at <= unix_secs(now) {
        0.0
    } else {
        w.used_percent
    }
}

/// When the window resets, in local time: `14:20` today, `Thu 09:00` within
/// a week, `reset` once it passed.
pub fn reset_label(resets_at: i64, now: SystemTime) -> String {
    let now_secs = unix_secs(now);
    if resets_at <= now_secs {
        return "reset".to_owned();
    }
    let (Some(at), Some(today)) = (local(resets_at), local(now_secs)) else {
        return String::new();
    };
    let hm = format!("{:02}:{:02}", at.tm_hour, at.tm_min);
    if at.tm_yday == today.tm_yday && at.tm_year == today.tm_year {
        hm
    } else {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        format!("{} {hm}", DAYS[at.tm_wday.rem_euclid(7) as usize])
    }
}

/// How long ago, short: `now`, `5m`, `3h`, `2d`.
pub fn age(observed_at_ms: u64, now: SystemTime) -> String {
    let secs = (unix_secs(now) - (observed_at_ms / 1000) as i64).max(0);
    match secs {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn local(secs: i64) -> Option<libc::tm> {
    let t = secs as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: localtime_r writes into the provided struct and returns null on failure.
    let ok = unsafe { !libc::localtime_r(&t, tm.as_mut_ptr()).is_null() };
    // SAFETY: initialized by localtime_r when ok.
    ok.then(|| unsafe { tm.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars() {
        assert_eq!(bar(0.0, 4), "▱▱▱▱");
        assert_eq!(bar(50.0, 4), "▰▰▱▱");
        assert_eq!(bar(140.0, 4), "▰▰▰▰");
    }
}
