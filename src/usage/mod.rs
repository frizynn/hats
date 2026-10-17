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

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::account::{Account, Harness, create_private_dir};
use crate::registry::{self, Switch};
use crate::store::Store;
use accounts::{Book, CLAUDE_UNASSIGNED, CODEX_UNASSIGNED, SwitchLine};
pub use prices::{PRICES_CHECKED, model_label};
pub use scan::Stats;
use scan::{Input, Roots, Tally};

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

/// Measured on 13.9 GB of logs (3,540 files), warm cache: 4 threads took
/// 2.87 s, 6 took 2.66 s and 8 took 2.61 s; reads of 256 KiB stay in the
/// CPU's cache and cost a third less CPU than reads of 1 MiB.
const THREADS: usize = 6;
const CHUNK: usize = 256 << 10;

/// Scans what the logs gained since the last scan and returns every day
/// counted so far. The first scan reads every log and takes seconds; later
/// ones read only what was appended. Two at once (the command and the view)
/// take turns.
pub fn report(store: &Store, now: SystemTime) -> Result<Report, String> {
    let dir = store.state.join("usage");
    create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join(".lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let sources = Sources::read(store, &dir, now)?;
    let today = Day::local(now);
    let mut tally = Tally::load(&dir, today);
    let stats = scan::scan(
        &mut tally,
        &Input {
            roots: &sources.roots,
            book: &sources.book,
            home: &store.home,
            day_of: local_day,
            chunk: CHUNK,
            threads: THREADS,
        },
    );
    tally
        .save(&dir, (unix_ms(now) / 86_400_000) as u16)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(build(&tally, &sources.labels, today, now, stats))
}

/// Whether a scan has saved totals before, so the first one can say it will
/// take a moment.
pub fn scanned_before(store: &Store) -> bool {
    store.state.join("usage/totals.json").exists()
}

fn build(
    tally: &Tally,
    labels: &HashMap<String, String>,
    today: Day,
    now: SystemTime,
    scan: Stats,
) -> Report {
    let t = &tally.totals;
    let mut rows: Vec<Row> = t
        .sums
        .iter()
        .filter(|(key, _)| key.day <= today)
        .map(|(key, sum)| {
            let account = t.name(key.account);
            let model = t.name(key.model);
            let project_id = t.name(key.repo);
            let tokens = Tokens::from_sum(sum);
            Row {
                date: key.day,
                harness: key.harness,
                account: account.to_owned(),
                account_label: account_label(account, labels),
                model: model.to_owned(),
                project: t
                    .repos
                    .get(project_id)
                    .cloned()
                    .unwrap_or_else(|| "(unknown)".to_owned()),
                project_id: project_id.to_owned(),
                cost: prices::cost(model, &tokens),
                tokens,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        (
            a.date,
            a.harness.name(),
            &a.account_label,
            &a.model,
            &a.project,
        )
            .cmp(&(
                b.date,
                b.harness.name(),
                &b.account_label,
                &b.model,
                &b.project,
            ))
    });
    Report {
        scanned_at: now,
        today,
        logs_from: t.logs_from,
        tracked_from: t.tracked_from,
        prices_checked: PRICES_CHECKED,
        rows,
        scan,
    }
}

fn account_label(id: &str, labels: &HashMap<String, String>) -> String {
    if let Some(label) = labels.get(id) {
        return label.clone();
    }
    if id == CLAUDE_UNASSIGNED || id == CODEX_UNASSIGNED {
        return "unassigned".to_owned();
    }
    // An account hats does not know (any more): its id is all there is.
    let hex = id.split_once(':').map_or(id, |(_, h)| h);
    hex.get(..8).unwrap_or(hex).to_owned()
}

/// Who was signed in when, where the logs are, and what each account is
/// called.
struct Sources {
    book: Book,
    roots: Roots,
    labels: HashMap<String, String>,
}

/// The sightings the scan writes: one line per config dir whose signed-in
/// account changed since the last one, in hats' registry format.
const SIGHTINGS: &str = "accounts.jsonl";

impl Sources {
    fn read(store: &Store, dir: &Path, now: SystemTime) -> Result<Self, String> {
        let read = |path: &Path| fs::read_to_string(path).unwrap_or_default();
        let hats = accounts::parse_switch_log(&read(&store.registry()));
        let own = accounts::parse_switch_log(&read(&dir.join(SIGHTINGS)));
        let mut lines: Vec<SwitchLine> = own.iter().chain(&hats).cloned().collect();
        let mut labels = HashMap::new();
        let mut roots = Roots::default();
        let mut real = HashSet::new();

        for harness in Harness::ALL {
            let mut dirs: Vec<Account> = Account::all(harness, store).map_err(|e| e.to_string())?;
            let known: HashSet<PathBuf> = dirs.iter().map(|a| a.dir.clone()).collect();
            // Config dirs that hooks saw and that are not accounts any more.
            let mut extra = HashSet::new();
            for line in hats.iter().filter(|l| l.harness == harness) {
                let path = PathBuf::from(&line.config_dir);
                if !known.contains(&path) && extra.insert(path.clone()) {
                    dirs.push(Account::from_config_dir(harness, Some(&path), store));
                }
            }
            for account in &dirs {
                let config_dir = account.dir.to_string_lossy().into_owned();
                let id = account.provider_id(store).ok().flatten();
                if let Some(id) = &id {
                    let full = format!("{}:{id}", harness.name());
                    // A directory hats does not manage has no label, only a path.
                    let label = if known.contains(&account.dir) {
                        labels.insert(full.clone(), account.label.clone());
                        account.label.as_str()
                    } else {
                        ""
                    };
                    if harness == Harness::Claude {
                        let last = own
                            .iter()
                            .rev()
                            .find(|l| l.config_dir == config_dir && l.session.is_none());
                        if last.is_none_or(|l| l.account != full) {
                            let line =
                                Switch::new(now, harness, id, label, &account.dir, None, None);
                            registry::append(&dir.join(SIGHTINGS), &line)
                                .map_err(|e| e.to_string())?;
                            lines.push(SwitchLine {
                                at: unix_ms(now),
                                harness,
                                account: full,
                                label: (!label.is_empty()).then(|| label.to_owned()),
                                config_dir: config_dir.clone(),
                                session: None,
                            });
                        }
                    }
                }
                match harness {
                    Harness::Claude => {
                        // hats' accounts link `projects` to one place: count it once.
                        if let Ok(projects) = fs::canonicalize(account.dir.join("projects"))
                            && real.insert(projects.clone())
                        {
                            roots.claude.push((config_dir, projects));
                        }
                    }
                    Harness::Codex => {
                        if let Ok(home) = fs::canonicalize(&account.dir)
                            && real.insert(home.clone())
                        {
                            roots.codex.push(home);
                        }
                    }
                }
            }
        }

        let default_claude = Account::default_for(Harness::Claude, store).dir;
        let default_claude = default_claude.to_string_lossy().into_owned();
        let mut swap_dirs = HashSet::new();
        let swap_log = store.home.join(".claude-swap-backup/claude-swap.log");
        if let Ok(text) = fs::read_to_string(&swap_log) {
            lines.extend(accounts::parse_claude_swap_log(
                &text,
                &default_claude,
                local_ms,
            ));
            swap_dirs.insert(default_claude);
        }
        let book = Book::new(&lines, &swap_dirs);
        for (id, label) in &book.labels {
            labels.entry(id.clone()).or_insert_with(|| label.clone());
        }
        Ok(Self {
            book,
            roots,
            labels,
        })
    }
}

fn unix_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The local calendar day of a Unix millisecond.
fn local_day(ms: i64) -> Day {
    let secs = ms.div_euclid(1000) as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: localtime_r fills `tm` and touches nothing else; on failure it
    // returns null and `tm` stays zeroed.
    let tm = unsafe {
        if libc::localtime_r(&secs, tm.as_mut_ptr()).is_null() {
            return Day((ms.div_euclid(86_400_000)) as i32);
        }
        tm.assume_init()
    };
    Day::from_ymd(
        i64::from(tm.tm_year) + 1900,
        (tm.tm_mon + 1) as u32,
        tm.tm_mday as u32,
    )
}

/// Unix milliseconds of a local wall-clock time: year, month, day, hour,
/// minute, second, millisecond.
fn local_ms(f: [i64; 7]) -> Option<i64> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = (f[0] - 1900) as i32;
    tm.tm_mon = (f[1] - 1) as i32;
    tm.tm_mday = f[2] as i32;
    tm.tm_hour = f[3] as i32;
    tm.tm_min = f[4] as i32;
    tm.tm_sec = f[5] as i32;
    // The zone's rules say whether daylight saving applied.
    tm.tm_isdst = -1;
    // SAFETY: mktime reads and normalizes `tm`, nothing else.
    let secs = unsafe { libc::mktime(&mut tm) };
    (secs != -1).then(|| secs * 1000 + f[6])
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
