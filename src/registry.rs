//! The account registry: an append-only JSONL file with one line each time an
//! account becomes active, read by usage tools to attribute tokens to accounts.
//!
//! Line format, version 1 (fixed by `tests/registry_format.rs`):
//!
//! ```json
//! {"v":1,"at":"2026-01-02T03:04:05.678Z","harness":"claude","account":"0123456789abcdef","label":"work","configDir":"/home/user/.local/share/hats/claude/work","session":"<id>"}
//! ```
//!
//! `session` is present when the line comes from a harness session hook and
//! binds only that session; without it the line moves the default account.
//! `reason` (`manual`, `threshold` or `limit`) says why the account changed;
//! it is absent on session lines that did not change anything.
//! Readers attribute a message of session S at time t to the newest line with
//! `session == S` and `at <= t`, else to the newest line without `session` and
//! `at <= t`, else to nobody.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::account::Harness;
use crate::policy::Reason;

pub const FILE_NAME: &str = "switches.jsonl";
const VERSION: u32 = 1;
/// The registry grows with every session; recent changes sit in its tail.
const RECENT_TAIL: u64 = 64 * 1024;

#[derive(Serialize)]
pub struct Switch<'a> {
    v: u32,
    at: String,
    harness: Harness,
    account: &'a str,
    label: &'a str,
    #[serde(rename = "configDir")]
    config_dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<Reason>,
}

impl<'a> Switch<'a> {
    pub fn new(
        at: SystemTime,
        harness: Harness,
        account: &'a str,
        label: &'a str,
        config_dir: &Path,
        session: Option<&'a str>,
        reason: Option<Reason>,
    ) -> Self {
        Self {
            v: VERSION,
            at: utc_millis(at),
            harness,
            account,
            label,
            config_dir: config_dir.to_string_lossy().into_owned(),
            session,
            reason,
        }
    }

    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("plain struct always serializes");
        line.push('\n');
        line
    }
}

/// Appends one line with a single `write(2)` on an `O_APPEND` descriptor.
/// POSIX moves the offset to the end and writes with no intervening
/// modification, so concurrent hooks never interleave their lines.
pub fn append(path: &Path, switch: &Switch) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(switch.to_line().as_bytes())
}

/// A change of account read back from the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// Unix seconds.
    pub at: i64,
    pub harness: Harness,
    /// The account before, when the registry's tail says.
    pub from: Option<String>,
    pub to: String,
    pub reason: Reason,
    /// A running session moved, rather than the default for new sessions.
    pub session: bool,
}

#[derive(Deserialize)]
struct Recorded {
    at: String,
    harness: Harness,
    label: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    reason: Option<Reason>,
}

/// The newest `limit` changes of account, oldest first.
pub fn recent(path: &Path, limit: usize) -> Vec<Change> {
    let mut tail = Vec::new();
    let start = File::open(path).and_then(|mut file| {
        let start = file.metadata()?.len().saturating_sub(RECENT_TAIL);
        file.seek(SeekFrom::Start(start))?;
        file.read_to_end(&mut tail)?;
        Ok(start)
    });
    let Ok(start) = start else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&tail);
    // A tail that starts mid-file starts mid-line.
    let lines = text.lines().skip(usize::from(start > 0));
    // Before its first line, a harness starts in its own directory.
    let mut defaults: HashMap<Harness, String> = HashMap::new();
    if start == 0 {
        for harness in Harness::ALL {
            defaults.insert(harness, crate::account::DEFAULT_LABEL.to_owned());
        }
    }
    let mut sessions: HashMap<String, String> = HashMap::new();
    let mut changes = Vec::new();
    for line in lines {
        let Ok(r) = serde_json::from_str::<Recorded>(line) else {
            continue;
        };
        let before = match &r.session {
            Some(session) => sessions.insert(session.clone(), r.label.clone()),
            None => defaults.insert(r.harness, r.label.clone()),
        };
        if let (Some(reason), Some(at)) = (r.reason, parse_utc(&r.at)) {
            changes.push(Change {
                at,
                harness: r.harness,
                from: before.filter(|b| *b != r.label),
                to: r.label,
                reason,
                session: r.session.is_some(),
            });
        }
    }
    changes.split_off(changes.len().saturating_sub(limit))
}

/// RFC 3339 UTC with milliseconds, e.g. `2026-01-02T03:04:05.678Z`.
pub fn utc_millis(t: SystemTime) -> String {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

/// Unix seconds of an RFC 3339 time such as `2026-10-16T19:29:59.824674+00:00`
/// or `2026-01-02T03:04:05.678Z`; the fraction is dropped.
pub fn parse_utc(text: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let b = text.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (month, day) = (num(5..7)?, num(8..10)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(num(0..4)?, month as u32, day as u32);
    let secs = days * 86_400 + num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19)?;
    let zone = text[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
            let at = text.len() - 5;
            let minutes = num(at..at + 2)? * 60 + num(at + 3..at + 5)?;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    Some(secs - offset * 60)
}

/// A proleptic Gregorian date to days since 1970-01-01 (Howard Hinnant,
/// "chrono-Compatible Low-Level Date Algorithms").
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant,
/// "chrono-Compatible Low-Level Date Algorithms").
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_epoch_leap_day_and_millis() {
        let at = |s: u64, ms: u64| UNIX_EPOCH + Duration::from_secs(s) + Duration::from_millis(ms);
        assert_eq!(utc_millis(at(0, 0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_millis(at(951_782_400, 5)), "2000-02-29T00:00:00.005Z");
        assert_eq!(
            utc_millis(at(1_767_323_045, 678)),
            "2026-01-02T03:04:05.678Z"
        );
        assert_eq!(
            utc_millis(at(4_102_444_799, 999)),
            "2099-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn recent_changes_say_from_where() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let at = |s: u64| UNIX_EPOCH + Duration::from_secs(1_767_323_000 + s);
        let dir_of = |label: &str| Path::new("/accounts").join(label);
        let lines = [
            Switch::new(
                at(0),
                Harness::Claude,
                "a",
                "work",
                &dir_of("work"),
                None,
                Some(Reason::Manual),
            ),
            Switch::new(
                at(1),
                Harness::Claude,
                "a",
                "work",
                &dir_of("work"),
                Some("s1"),
                None,
            ),
            Switch::new(
                at(2),
                Harness::Claude,
                "b",
                "side",
                &dir_of("side"),
                Some("s1"),
                Some(Reason::Limit),
            ),
            Switch::new(
                at(3),
                Harness::Codex,
                "c",
                "alt",
                &dir_of("alt"),
                None,
                Some(Reason::Threshold),
            ),
            Switch::new(
                at(4),
                Harness::Claude,
                "d",
                "spare",
                &dir_of("spare"),
                None,
                Some(Reason::Threshold),
            ),
        ];
        for line in &lines {
            append(&path, line).unwrap();
        }
        let summary: Vec<_> = recent(&path, 3)
            .into_iter()
            .map(|c| (c.from, c.to, c.session))
            .collect();
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(
            summary,
            [
                (some("work"), "side".to_owned(), true),
                (some("default"), "alt".to_owned(), false),
                (some("work"), "spare".to_owned(), false),
            ]
        );
        assert!(recent(&dir.path().join("none"), 3).is_empty());
    }

    #[test]
    fn parses_what_it_writes_and_offsets() {
        for secs in [0, 951_782_400, 1_767_323_045, 4_102_444_799] {
            let text = utc_millis(UNIX_EPOCH + Duration::from_secs(secs));
            assert_eq!(parse_utc(&text), Some(secs as i64), "{text}");
        }
        let at = Some(1_792_179_000);
        assert_eq!(parse_utc("2026-10-16T19:30:00.824674+00:00"), at);
        assert_eq!(parse_utc("2026-10-16T21:30:00+02:00"), at);
        assert_eq!(parse_utc("2026-10-16T16:30:00-03:00"), at);
        for bad in [
            "",
            "2026-10-16",
            "2026-13-01T00:00:00Z",
            "2026-10-16T19:30:00",
            "x026-10-16T19:30:00Z",
        ] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
    }
}
