//! Which account each message was billed to. A Codex rollout names it
//! (`creator_account_id`); a Claude Code transcript never does, so Claude use
//! is attributed from a time line of "account X became active" lines: hats'
//! `switches.jsonl`, the sightings the scan writes in the same format, and
//! claude-swap's log when it exists.
//!
//! Ids carry their harness, as in Nenu: `claude:<16 hex>`, `claude:swap-2`,
//! `codex:<16 hex>`, and `<harness>:unassigned`. A claude-swap slot is
//! counted under its number and shown as the account hats found in it.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use super::bytes::iso_ms_at;
use crate::account::Harness;

pub const CLAUDE_UNASSIGNED: &str = "claude:unassigned";
pub const CODEX_UNASSIGNED: &str = "codex:unassigned";
pub const SWAP_PREFIX: &str = "claude:swap-";

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchLine {
    /// Unix milliseconds.
    pub at: i64,
    pub harness: Harness,
    pub account: String,
    pub label: Option<String>,
    pub config_dir: String,
    pub session: Option<String>,
}

#[derive(Deserialize)]
struct RawLine {
    v: u32,
    at: String,
    harness: Harness,
    account: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(rename = "configDir")]
    config_dir: String,
    #[serde(default)]
    session: Option<String>,
}

/// Lines of a v1 switch log (`registry.rs` defines the format). A line that
/// does not parse or misses a field is skipped.
pub fn parse_switch_log(text: &str) -> Vec<SwitchLine> {
    text.lines()
        .filter_map(|line| {
            let r: RawLine = serde_json::from_str(line).ok()?;
            let at = iso_ms_at(r.at.as_bytes(), 0)?;
            let hex = r.account.len() == 16
                && r.account
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            (r.v == 1 && hex).then(|| SwitchLine {
                at,
                harness: r.harness,
                account: format!("{}:{}", r.harness.name(), r.account),
                label: r.label.filter(|l| !l.is_empty() && l.len() <= 64),
                config_dir: r.config_dir,
                session: r.session.filter(|s| s.len() <= 128),
            })
        })
        .collect()
}

/// claude-swap's log as switch lines for `config_dir`. Two kinds of line
/// matter: "Added account N" (it adds whoever is signed in, so N is active
/// then) and "Switched from account N to M". The slots are claude-swap's own
/// numbering, and its times are local wall-clock time.
pub fn parse_claude_swap_log(
    text: &str,
    config_dir: &str,
    local_ms: impl Fn([i64; 7]) -> Option<i64>,
) -> Vec<SwitchLine> {
    text.lines()
        .filter_map(|line| {
            let (time, slot) = swap_event(line)?;
            Some(SwitchLine {
                at: local_ms(time)?,
                harness: Harness::Claude,
                account: format!("{SWAP_PREFIX}{slot}"),
                label: None,
                config_dir: config_dir.to_owned(),
                session: None,
            })
        })
        .collect()
}

/// `2026-01-02 10:00:00,123 - INFO - Added account 1` to its wall-clock time
/// (year, month, day, hour, minute, second, millisecond) and slot.
fn swap_event(line: &str) -> Option<([i64; 7], &str)> {
    let b = line.as_bytes();
    let num = |from: usize, n: usize| -> Option<i64> {
        b.get(from..from + n)?.iter().try_fold(0i64, |v, &c| {
            c.is_ascii_digit().then(|| v * 10 + i64::from(c - b'0'))
        })
    };
    if b.len() < 19 || [b[4], b[7], b[10], b[13], b[16]] != *b"-- ::" {
        return None;
    }
    let mut time = [
        num(0, 4)?,
        num(5, 2)?,
        num(8, 2)?,
        num(11, 2)?,
        num(14, 2)?,
        num(17, 2)?,
        0,
    ];
    let mut rest = &line[19..];
    if let Some(frac) = rest.strip_prefix(',') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=3).contains(&digits) {
            return None;
        }
        time[6] = frac[..digits].parse::<i64>().ok()? * 10i64.pow(3 - digits as u32);
        rest = &frac[digits..];
    }
    let event = rest.strip_prefix(" - INFO - ")?;
    let slot = match event.strip_prefix("Added account ") {
        Some(slot) => slot,
        None => {
            let (from, to) = event
                .strip_prefix("Switched from account ")?
                .split_once(" to ")?;
            if from.is_empty() || !from.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            to
        }
    };
    let digits = slot.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| (time, &slot[..digits]))
}

/// The slot of claude-swap's copy of an account's `.claude.json`, named
/// `.claude-config-<slot>-<mail>.json`. The mail is left where it is.
pub fn swap_slot(file_name: &str) -> Option<&str> {
    let rest = file_name.strip_prefix(".claude-config-")?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && rest[digits..].starts_with('-') && rest.ends_with(".json"))
        .then(|| &rest[..digits])
}

pub fn codex_account(creator_account_id: Option<&str>) -> String {
    match creator_account_id {
        Some(id) if !id.is_empty() => format!("codex:{}", crate::account::short_hash(id)),
        _ => CODEX_UNASSIGNED.to_owned(),
    }
}

/// Times and accounts, oldest first.
#[derive(Default)]
struct Timeline(Vec<(i64, String)>);

impl Timeline {
    /// The account of the last line at or before `at`.
    fn at(&self, at: i64) -> Option<&str> {
        let i = self.0.partition_point(|(t, _)| *t <= at);
        i.checked_sub(1).map(|i| self.0[i].1.as_str())
    }
}

/// Message to account. A message of session S at time t belongs to the last
/// line naming S at or before t; else to the last line of its config dir
/// without a session; else, when the dir only ever showed one account, to
/// that one; else to nobody. Where claude-swap's log exists, its slots stand
/// for that dir: sessionless sightings there would name the same people
/// twice.
#[derive(Default)]
pub struct Book {
    sessions: HashMap<String, Timeline>,
    dirs: HashMap<String, Timeline>,
    only: HashMap<String, String>,
    /// The first label a line gave each account.
    pub labels: HashMap<String, String>,
}

impl Book {
    pub fn new(lines: &[SwitchLine], swap_dirs: &HashSet<String>) -> Self {
        let mut book = Book::default();
        let mut seen: HashMap<&str, HashSet<&str>> = HashMap::new();
        let mut sorted: Vec<&SwitchLine> = lines.iter().collect();
        sorted.sort_by_key(|l| l.at);
        for line in lines {
            if let Some(label) = &line.label {
                book.labels
                    .entry(line.account.clone())
                    .or_insert_with(|| label.clone());
            }
        }
        for line in sorted {
            if line.harness != Harness::Claude {
                continue;
            }
            seen.entry(&line.config_dir)
                .or_default()
                .insert(&line.account);
            let timeline = match &line.session {
                Some(session) => book.sessions.entry(session.clone()).or_default(),
                None if !swap_dirs.contains(&line.config_dir)
                    || line.account.starts_with(SWAP_PREFIX) =>
                {
                    book.dirs.entry(line.config_dir.clone()).or_default()
                }
                None => continue,
            };
            timeline.0.push((line.at, line.account.clone()));
        }
        for (dir, accounts) in seen {
            if accounts.len() == 1 && !swap_dirs.contains(dir) {
                let only = accounts.into_iter().next().expect("one account");
                book.only.insert(dir.to_owned(), only.to_owned());
            }
        }
        book
    }

    pub fn claude(&self, config_dir: &str, session: Option<&str>, at: i64) -> &str {
        session
            .and_then(|s| self.sessions.get(s))
            .and_then(|t| t.at(at))
            .or_else(|| self.dirs.get(config_dir).and_then(|t| t.at(at)))
            .or_else(|| self.only.get(config_dir).map(String::as_str))
            .unwrap_or(CLAUDE_UNASSIGNED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // hats' own lines, in the format its registry writes (fake ids).
    const HATS: &str = r#"{"v":1,"at":"2026-01-02T03:04:05.678Z","harness":"claude","account":"9ae25bfbd05c9e76","label":"work","configDir":"/home/user/.local/share/hats/claude/work","reason":"manual"}
{"v":1,"at":"2026-01-02T03:10:00.000Z","harness":"claude","account":"9ae25bfbd05c9e76","label":"work","configDir":"/home/user/.local/share/hats/claude/work","session":"s1"}
{"v":1,"at":"2026-01-02T04:00:00.001Z","harness":"claude","account":"0123456789abcdef","label":"side","configDir":"/home/user/.local/share/hats/claude/side","session":"s1"}
{"v":1,"at":"2026-01-02T05:00:00.000Z","harness":"codex","account":"9a00cf1657c91b7f","label":"personal","configDir":"/home/user/.local/share/hats/codex/personal"}
not json
{"v":2,"at":"2026-01-02T00:00:00.000Z","harness":"claude","account":"0123456789abcdef","configDir":"/x"}
{"v":1,"at":"2026-01-02T00:00:00.000Z","harness":"claude","account":"user@example.com","configDir":"/x"}"#;

    const WORK: &str = "/home/user/.local/share/hats/claude/work";
    const DEFAULT: &str = "/home/user/.claude";

    fn ms(iso: &str) -> i64 {
        iso_ms_at(iso.as_bytes(), 0).unwrap()
    }

    /// Wall-clock time read as UTC, for tests that do not depend on the zone.
    fn as_utc(f: [i64; 7]) -> Option<i64> {
        let days = crate::registry::days_from_civil(f[0], f[1] as u32, f[2] as u32);
        Some((days * 86_400 + f[3] * 3600 + f[4] * 60 + f[5]) * 1000 + f[6])
    }

    #[test]
    fn hats_lines_parse_and_broken_ones_are_skipped() {
        let lines = parse_switch_log(HATS);
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[2],
            SwitchLine {
                at: ms("2026-01-02T04:00:00.001Z"),
                harness: Harness::Claude,
                account: "claude:0123456789abcdef".into(),
                label: Some("side".into()),
                config_dir: "/home/user/.local/share/hats/claude/side".into(),
                session: Some("s1".into()),
            }
        );
        assert_eq!(lines[3].account, "codex:9a00cf1657c91b7f");
    }

    #[test]
    fn a_session_line_wins_and_a_later_one_takes_over() {
        let book = Book::new(&parse_switch_log(HATS), &HashSet::new());
        let work = "claude:9ae25bfbd05c9e76";
        assert_eq!(
            book.claude(WORK, Some("s1"), ms("2026-01-02T03:30:00Z")),
            work
        );
        // A resume under another account.
        assert_eq!(
            book.claude(WORK, Some("s1"), ms("2026-01-02T04:30:00Z")),
            "claude:0123456789abcdef"
        );
        // Without a line of its own, the config dir's.
        assert_eq!(
            book.claude(WORK, Some("other"), ms("2026-01-02T03:05:00Z")),
            work
        );
        // A dir that only ever showed one account owns its earlier history.
        assert_eq!(book.claude(WORK, None, ms("2025-12-01T00:00:00Z")), work);
        assert_eq!(
            book.claude(DEFAULT, None, ms("2026-01-03T00:00:00Z")),
            CLAUDE_UNASSIGNED
        );
        assert_eq!(book.labels["claude:9ae25bfbd05c9e76"], "work");
    }

    #[test]
    fn claude_swap_names_the_account_added_or_switched_to() {
        let log = "2026-01-02 10:00:00,123 - INFO - Added account 1\n\
                   2026-01-02 10:05:00,000 - DEBUG - Usage API something\n\
                   2026-01-02 11:00:00 - INFO - Switched from account 1 to 2\n\
                   garbage\n";
        let lines = parse_claude_swap_log(log, DEFAULT, as_utc);
        let got: Vec<_> = lines.iter().map(|l| (l.at, l.account.as_str())).collect();
        assert_eq!(
            got,
            [
                (ms("2026-01-02T10:00:00.123Z"), "claude:swap-1"),
                (ms("2026-01-02T11:00:00Z"), "claude:swap-2"),
            ]
        );
        let book = Book::new(&lines, &HashSet::from([DEFAULT.to_owned()]));
        let at = |iso: &str| book.claude(DEFAULT, Some("s"), ms(iso)).to_owned();
        assert_eq!(at("2026-01-02T09:00:00Z"), CLAUDE_UNASSIGNED);
        assert_eq!(at("2026-01-02T10:30:00Z"), "claude:swap-1");
        assert_eq!(at("2026-01-02T12:00:00Z"), "claude:swap-2");
    }

    #[test]
    fn with_claude_swap_a_sighting_of_its_dir_does_not_add_a_second_name() {
        let mut lines = parse_switch_log(
            r#"{"v":1,"at":"2026-01-02T12:30:00.000Z","harness":"claude","account":"0123456789abcdef","configDir":"/home/user/.claude"}"#,
        );
        lines.extend(parse_claude_swap_log(
            "2026-01-02 11:00:00,000 - INFO - Switched from account 1 to 2",
            DEFAULT,
            as_utc,
        ));
        let book = Book::new(&lines, &HashSet::from([DEFAULT.to_owned()]));
        assert_eq!(
            book.claude(DEFAULT, None, ms("2026-01-03T00:00:00Z")),
            "claude:swap-2"
        );
        // Without claude-swap, one sighting owns the dir's whole history.
        let alone = Book::new(&lines[..1], &HashSet::new());
        assert_eq!(
            alone.claude(DEFAULT, None, ms("2025-06-01T00:00:00Z")),
            "claude:0123456789abcdef"
        );
    }

    #[test]
    fn only_the_slot_is_read_from_the_name_of_a_claude_swap_copy() {
        assert_eq!(
            swap_slot(".claude-config-3-user@example.com.json"),
            Some("3")
        );
        assert_eq!(
            swap_slot(".claude-config-12-a-b@example.com.json"),
            Some("12")
        );
        for other in [
            ".claude-config--user@example.com.json",
            ".claude-config-3.json",
            ".claude-config-3-user@example.com.json.tmp",
            "claude-config-3-user@example.com.json",
        ] {
            assert_eq!(swap_slot(other), None, "{other}");
        }
    }

    #[test]
    fn a_rollout_without_creator_is_unassigned() {
        assert_eq!(codex_account(None), CODEX_UNASSIGNED);
        assert_eq!(
            codex_account(Some("acct-1")),
            format!("codex:{}", crate::account::short_hash("acct-1"))
        );
    }
}
