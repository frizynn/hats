//! Token use of Claude Code and Codex, read from the agents' own logs, and
//! what it would cost at API list price.
//!
//! Nothing runs until asked: [`report`] scans what the logs gained since the
//! last scan, saves the totals under `<state>/usage/` and returns them as
//! daily rows. The rules are Nenu's (its ADR 0085): Claude rows deduplicated
//! by message id and request id across logs, Codex rows from the difference
//! of cumulative totals, offsets per log of whole lines only, tokens kept and
//! priced when read. Never a credential, never a usage endpoint, never an
//! email in the state.

mod accounts;
mod bytes;
mod prices;
mod repos;
mod scan;
pub mod summary;

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::account::Harness;
use crate::registry;
use crate::store::Store;
pub use prices::{PRICES_CHECKED, model_label};
pub use scan::Stats;

/// A calendar day in local time, as days since 1970-01-01.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(pub i32);

impl Day {
    pub fn local(t: SystemTime) -> Self {
        local_day(unix_ms(t))
    }

    pub fn from_ymd(year: i64, month: u32, day: u32) -> Self {
        Self(registry::days_from_civil(year, month, day) as i32)
    }

    pub fn ymd(self) -> (i64, u32, u32) {
        registry::civil_from_days(i64::from(self.0))
    }

    pub fn month_start(self) -> Self {
        let (y, m, _) = self.ymd();
        Self::from_ymd(y, m, 1)
    }

    /// `days` before this one.
    pub fn minus(self, days: i32) -> Self {
        Self(self.0 - days)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let b = text.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        let (y, m, d) = (
            text[..4].parse().ok()?,
            text[5..7].parse().ok()?,
            text[8..].parse().ok()?,
        );
        ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| Self::from_ymd(y, m, d))
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

impl Serialize for Day {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Tokens of one kind of use, in Nenu's columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub requests: u64,
    /// Uncached input.
    pub input: u64,
    /// Output, reasoning included.
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    /// The part of output that was reasoning, when the harness says (Codex).
    pub reasoning: u64,
}

impl Tokens {
    fn from_sum(s: &scan::Sum) -> Self {
        Self {
            requests: s[0],
            input: s[1],
            output: s[2],
            cache_write_5m: s[3],
            cache_write_1h: s[4],
            cache_read: s[5],
            reasoning: s[6],
        }
    }

    /// Input, output and cache, each token once.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }
}

/// One day's use of one model by one account in one project.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub date: Day,
    pub harness: Harness,
    /// `claude:<16 hex>`, `claude:swap-<n>`, `codex:<16 hex>` or
    /// `<harness>:unassigned`.
    pub account: String,
    /// The account's hats label, else one from the switch logs.
    pub account_label: String,
    /// The harness's model id; `+fast` and `+long` mark Claude's fast mode
    /// and Haiku's long prompts, which have their own price.
    pub model: String,
    /// The repository's folder name.
    pub project: String,
    /// The folder name and a hash of the repository's path.
    pub project_id: String,
    #[serde(flatten)]
    pub tokens: Tokens,
    /// USD at API list price, or `None` for a model without a price.
    pub cost: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    #[serde(serialize_with = "as_utc")]
    pub scanned_at: SystemTime,
    pub today: Day,
    /// The earliest day any log had: Claude Code deletes transcripts after
    /// 30 days, and what hats counted before that stays.
    pub logs_from: Option<Day>,
    /// The first day hats kept totals of its own.
    pub tracked_from: Day,
    pub prices_checked: &'static str,
    /// Sorted by date, harness, account label, model and project; days after
    /// today are left out.
    pub rows: Vec<Row>,
    pub scan: Stats,
}

fn as_utc<S: serde::Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&registry::utc_millis(*t))
}

const THREADS: usize = 4;
const CHUNK: usize = 1 << 20;

/// Scans what the logs gained since the last scan and returns every day
/// counted so far.
pub fn report(_store: &Store, _now: SystemTime) -> Result<Report, String> {
    Err("not counted yet".to_owned())
}

/// Whether a scan has saved totals before, so the first one can say it will
/// take a moment.
pub fn scanned_before(store: &Store) -> bool {
    store.state.join("usage/totals.json").exists()
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The local calendar day of a Unix millisecond.
fn local_day(ms: i64) -> Day {
    Day(ms.div_euclid(86_400_000) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_print_parse_and_count() {
        let day = Day::parse("2026-03-01").unwrap();
        assert_eq!(day.to_string(), "2026-03-01");
        assert_eq!(day.minus(1).to_string(), "2026-02-28");
        assert_eq!(day.minus(1).month_start().to_string(), "2026-02-01");
        assert_eq!(Day::parse("2026-13-01"), None);
        assert_eq!(Day::parse("2026-1-01"), None);
    }
}
