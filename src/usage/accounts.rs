//! Which account each message was billed to. A Codex rollout names it
//! (`creator_account_id`); a Claude Code transcript never does, so Claude use
//! is attributed from a time line of "account X became active" lines: hats'
//! `switches.jsonl`, the sightings the scan writes in the same format, and
//! claude-swap's log when it exists.
//!
//! Ids carry their harness, as in Nenu: `claude:<16 hex>`, `claude:swap-2`,
//! `codex:<16 hex>`, and `<harness>:unassigned`.

use std::collections::{HashMap, HashSet};

use crate::account::Harness;

pub const CLAUDE_UNASSIGNED: &str = "claude:unassigned";
pub const CODEX_UNASSIGNED: &str = "codex:unassigned";

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

/// Lines of a v1 switch log (`registry.rs` defines the format). A line that
/// does not parse or misses a field is skipped.
pub fn parse_switch_log(_text: &str) -> Vec<SwitchLine> {
    Vec::new()
}

/// claude-swap's log as switch lines for `config_dir`.
pub fn parse_claude_swap_log(
    _text: &str,
    _config_dir: &str,
    _local_ms: impl Fn([i64; 7]) -> Option<i64>,
) -> Vec<SwitchLine> {
    Vec::new()
}

pub fn codex_account(_creator_account_id: Option<&str>) -> String {
    CODEX_UNASSIGNED.to_owned()
}

/// Message to account: the session's line, else the config dir's, else the
/// dir's only account, else nobody.
#[derive(Default)]
pub struct Book {
    /// The first label a line gave each account.
    pub labels: HashMap<String, String>,
}

impl Book {
    pub fn new(_lines: &[SwitchLine], _swap_dirs: &HashSet<String>) -> Self {
        Book::default()
    }

    pub fn claude(&self, _config_dir: &str, _session: Option<&str>, _at: i64) -> &str {
        CLAUDE_UNASSIGNED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::bytes::iso_ms_at;

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
        let got: Vec<_> = lines
            .iter()
            .map(|l| (l.at, l.account.as_str(), l.label.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    ms("2026-01-02T10:00:00.123Z"),
                    "claude:swap-1",
                    Some("Account 1")
                ),
                (
                    ms("2026-01-02T11:00:00Z"),
                    "claude:swap-2",
                    Some("Account 2")
                ),
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
    fn a_rollout_without_creator_is_unassigned() {
        assert_eq!(codex_account(None), CODEX_UNASSIGNED);
        assert_eq!(
            codex_account(Some("acct-1")),
            format!("codex:{}", crate::account::short_hash("acct-1"))
        );
    }
}
