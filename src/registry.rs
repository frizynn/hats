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
//! Readers attribute a message of session S at time t to the newest line with
//! `session == S` and `at <= t`, else to the newest line without `session` and
//! `at <= t`, else to nobody.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::account::Harness;

pub const FILE_NAME: &str = "switches.jsonl";
const VERSION: u32 = 1;

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
}

impl<'a> Switch<'a> {
    pub fn new(
        at: SystemTime,
        harness: Harness,
        account: &'a str,
        label: &'a str,
        config_dir: &Path,
        session: Option<&'a str>,
    ) -> Self {
        Self {
            v: VERSION,
            at: utc_millis(at),
            harness,
            account,
            label,
            config_dir: config_dir.to_string_lossy().into_owned(),
            session,
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

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant,
/// "chrono-Compatible Low-Level Date Algorithms").
fn civil_from_days(days: i64) -> (i64, u32, u32) {
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
}
