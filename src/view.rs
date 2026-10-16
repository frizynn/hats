//! What `hats list` and the interactive view show, laid out for a width:
//! one block per harness with a header in words, column titles and one or
//! two lines per account.

use std::time::SystemTime;

use crate::account::Harness;
use crate::auto::Status;
use crate::config::{HarnessConfig, Mode, Strategy};
use crate::limits::{LIMIT_HOLD, LimitWindow, Reading, unix_secs};
use crate::policy;

/// Marks the account new sessions start with.
pub const MARK: &str = "●";
/// A reading older than this is drawn dim: no session of that account has
/// repainted its status line since, so the numbers may have moved.
const STALE_SECS: i64 = 60 * 60;
/// One window besides its bar: ` 100% Tue 21:00`.
const WINDOW_TEXT: usize = 15;
/// `12 min ago`, `3 days ago`.
const AGE_WIDTH: usize = 10;
const GAP: &str = "  ";

/// How a piece of text looks; plain text output ignores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    Strong,
    /// The harness name and the mark.
    Title,
    Good,
    Warn,
    Bad,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
}

pub type Row = Vec<Cell>;

fn cell(text: impl Into<String>, tone: Tone) -> Cell {
    Cell {
        text: text.into(),
        tone,
    }
}

/// The row as plain text, without trailing spaces.
pub fn plain(row: &Row) -> String {
    let text: String = row.iter().map(|c| c.text.as_str()).collect();
    text.trim_end().to_owned()
}

/// Whether to name keys (interactive view) or commands (`hats list`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hints {
    Keys,
    Commands,
}

/// One harness: its header rows and, per account, its rows.
pub struct Block {
    pub header: Vec<Row>,
    pub accounts: Vec<Vec<Row>>,
}

/// Column widths for one block.
struct Layout {
    label: usize,
    /// The identity column, when it fits on the account's line; otherwise it
    /// goes on a second line.
    who: Option<usize>,
    bar: usize,
}

impl Layout {
    fn new(statuses: &[Status], harness: Harness, width: usize) -> Self {
        let label = statuses
            .iter()
            .map(|s| s.account.label.chars().count())
            .max()
            .unwrap_or(0)
            .max("ACCOUNT".len());
        let who = statuses
            .iter()
            .map(|s| who(s, harness).chars().count())
            .max()
            .unwrap_or(0)
            .max("SIGNED IN AS".len());
        // Marker, label, two windows, age, and the gaps between them.
        let fixed = 2 + label + 3 * GAP.len() + 2 * WINDOW_TEXT + GAP.len() + AGE_WIDTH;
        let bars = |room: usize| room / 2;
        let wide = fixed + who + GAP.len();
        if width >= wide + 2 * 6 {
            Self {
                label,
                who: Some(who),
                bar: bars(width - wide).min(10),
            }
        } else {
            Self {
                label,
                who: None,
                bar: bars(width.saturating_sub(fixed)).clamp(3, 10),
            }
        }
    }

    fn window(&self) -> usize {
        self.bar + WINDOW_TEXT
    }
}

/// Where the login is and who it is: `~/.claude · dana@example.com · Max`.
pub fn who(s: &Status, harness: Harness) -> String {
    let home = s.account.is_default.then(|| home_dir(harness).to_owned());
    let id = &s.identity;
    [
        home,
        id.email.clone(),
        id.plan.clone(),
        id.organization.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The harness's own directory, which the account named `default` uses.
pub fn home_dir(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "~/.claude",
        Harness::Codex => "~/.codex",
    }
}

pub fn title(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "CLAUDE CODE",
        Harness::Codex => "CODEX",
    }
}

/// What auto mode does, in one sentence.
pub fn mode_sentence(c: &HarnessConfig) -> String {
    match c.mode {
        Mode::Manual => "Manual: it changes only when you choose another account.".to_owned(),
        Mode::Auto => format!(
            "Auto: at {} % of either limit, it moves to {}.",
            c.threshold,
            match c.strategy {
                Strategy::Headroom => "the account with the most room",
                Strategy::Next => "the next account with room",
            }
        ),
    }
}

pub fn block(
    harness: Harness,
    config: &HarnessConfig,
    statuses: &[Status],
    current: Option<&str>,
    width: usize,
    now: SystemTime,
    hints: Hints,
) -> Block {
    let layout = Layout::new(statuses, harness, width);
    let mut header = vec![vec![
        cell(title(harness), Tone::Title),
        cell(" · new sessions start in ", Tone::Plain),
        cell(MARK, Tone::Title),
        cell(format!(" {}", current.unwrap_or("default")), Tone::Strong),
    ]];
    header.extend(wrap(&mode_sentence(config), width).map(|line| vec![cell(line, Tone::Plain)]));
    let mut titles = format!("  {:<w$}{GAP}", "ACCOUNT", w = layout.label);
    if let Some(w) = layout.who {
        titles += &format!("{:<w$}{GAP}", "SIGNED IN AS");
    }
    let w = layout.window();
    titles += &format!(
        "{:<w$}{GAP}{:<w$}{GAP}UPDATED",
        "5-HOUR LIMIT", "WEEKLY LIMIT"
    );
    header.push(vec![cell(titles, Tone::Dim)]);

    let accounts = statuses
        .iter()
        .map(|s| {
            let is_current = current == Some(s.account.label.as_str());
            let mut line = vec![
                if is_current {
                    cell(format!("{MARK} "), Tone::Title)
                } else {
                    cell("  ", Tone::Plain)
                },
                cell(
                    format!("{:<w$}{GAP}", s.account.label, w = layout.label),
                    if is_current {
                        Tone::Strong
                    } else {
                        Tone::Plain
                    },
                ),
            ];
            let who = who(s, harness);
            if let Some(w) = layout.who {
                line.push(cell(format!("{who:<w$}{GAP}"), Tone::Dim));
            }
            line.extend(state(s, harness, config, &layout, now, hints));
            let mut rows = vec![line];
            if layout.who.is_none() && !who.is_empty() {
                // Under the label and a step in, so it reads as part of it.
                rows.push(vec![cell(
                    format!("    {}", fit(&who, width.saturating_sub(4))),
                    Tone::Dim,
                )]);
            }
            rows
        })
        .collect();
    Block { header, accounts }
}

/// The limits, or why there are none.
fn state(
    s: &Status,
    harness: Harness,
    config: &HarnessConfig,
    layout: &Layout,
    now: SystemTime,
    hints: Hints,
) -> Row {
    let Some(_) = &s.id else {
        let how = match hints {
            Hints::Keys => "press l to sign in".to_owned(),
            Hints::Commands => format!(
                "hats{} login {}",
                if harness == Harness::Codex {
                    " --codex"
                } else {
                    ""
                },
                s.account.label
            ),
        };
        return vec![cell(format!("not signed in: {how}"), Tone::Warn)];
    };
    let Some(r) = &s.reading else {
        return vec![cell(
            "no data yet: appears once you use this account",
            Tone::Dim,
        )];
    };
    if r.limited(now) {
        let until = r.limited_until.expect("limited");
        let hit = until - LIMIT_HOLD.as_secs() as i64;
        return vec![cell(
            format!(
                "hit its limit at {}: counted as full until {}",
                clock(hit, now),
                clock(until, now)
            ),
            Tone::Bad,
        )];
    }
    let stale = unix_secs(now) - (r.observed_at / 1000) as i64 > STALE_SECS;
    let mut row = Row::new();
    for name in ["five_hour", "seven_day"] {
        match r.windows.iter().find(|w| w.name == name) {
            Some(w) => row.extend(window(w, layout.bar, config.threshold, stale, now)),
            None => row.push(cell(
                format!("{:<w$}", "  –", w = layout.window()),
                Tone::Dim,
            )),
        }
        row.push(cell(GAP, Tone::Plain));
    }
    row.push(cell(
        age(r, now),
        if stale { Tone::Dim } else { Tone::Plain },
    ));
    row
}

fn window(w: &LimitWindow, bar_cells: usize, threshold: u8, stale: bool, now: SystemTime) -> Row {
    let used = used_now(w, now);
    let reset = w.resets_at <= unix_secs(now);
    let quiet = stale || reset;
    vec![
        cell(
            bar(used, bar_cells),
            if quiet {
                Tone::Dim
            } else {
                level(used, threshold)
            },
        ),
        cell(
            format!(" {used:>3.0}% {:<9}", reset_label(w.resets_at, now)),
            if quiet { Tone::Dim } else { Tone::Plain },
        ),
    ]
}

/// Red past the threshold; yellow within the margin under it, where the
/// account can no longer be picked to switch to; green below.
fn level(used: f64, threshold: u8) -> Tone {
    let threshold = f64::from(threshold);
    if used >= threshold {
        Tone::Bad
    } else if used > threshold - policy::MARGIN {
        Tone::Warn
    } else {
        Tone::Good
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
    if resets_at <= unix_secs(now) {
        return "reset".to_owned();
    }
    clock(resets_at, now)
}

/// A local time: `14:20` today, `Thu 09:00` on another day.
pub fn clock(secs: i64, now: SystemTime) -> String {
    let (Some(at), Some(today)) = (local(secs), local(unix_secs(now))) else {
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

/// How old a reading is: `just now`, `5 min ago`, `3 h ago`, `2 days ago`.
pub fn age(r: &Reading, now: SystemTime) -> String {
    ago(r.observed_at / 1000, now)
}

/// How long ago `secs` (Unix seconds) was, in words.
pub fn ago(secs: u64, now: SystemTime) -> String {
    let elapsed = (unix_secs(now) - secs as i64).max(0);
    match elapsed {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", elapsed / 60),
        3600..172_800 => format!("{} h ago", elapsed / 3600),
        _ => format!("{} days ago", elapsed / 86_400),
    }
}

/// `text` cut to `width` characters, with `…` when cut.
pub fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Words of `text` in lines of at most `width` characters.
fn wrap(text: &str, width: usize) -> impl Iterator<Item = String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split(' ') {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines.into_iter()
}

fn local(secs: i64) -> Option<libc::tm> {
    // `time_t` is 64 bits on every target hats builds for; naming
    // `libc::time_t` is deprecated on musl, so `localtime_r` takes the i64.
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: localtime_r writes into the provided struct and returns null on failure.
    let ok = unsafe { !libc::localtime_r(&secs, tm.as_mut_ptr()).is_null() };
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

    #[test]
    fn ages_in_words() {
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let at = |ago_secs: u64| ago(1_000_000 - ago_secs, now);
        assert_eq!(at(5), "just now");
        assert_eq!(at(125), "2 min ago");
        assert_eq!(at(3 * 3600), "3 h ago");
        assert_eq!(at(3 * 86_400), "3 days ago");
    }

    #[test]
    fn wraps_and_fits() {
        let lines: Vec<_> = wrap("aa bb cc", 5).collect();
        assert_eq!(lines, ["aa bb", "cc"]);
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
    }
}
