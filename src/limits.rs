//! Last known plan limits per account, only from what the harnesses already
//! give: the `rate_limits` of Claude Code's status line input (kept in
//! `limits.json` by the status line wrapper) and the `rate_limits` of Codex's
//! `token_count` events in each account's rollouts.
//!
//! `limits.json`, version 1:
//!
//! ```json
//! {"v":1,"accounts":[{"harness":"claude","account":"0123456789abcdef","label":"work","observedAt":1767323045678,"windows":[{"name":"five_hour","usedPercent":23.5,"resetsAt":1767330000}]}]}
//! ```
//!
//! `limitedUntil` (Unix seconds), when present, says a turn of that account
//! ended with a rate limit error: it counts as full until then, or until its
//! status line reports other numbers.
//!
//! `loginRefusedAt` (Unix seconds), when present, says the provider refused
//! the account's login when its limits were last asked for (expired or
//! revoked): it has to sign in again. The next reading that works clears it.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::account::{Account, Harness, newest_rollout};
use crate::store::Store;

pub const FILE_NAME: &str = "limits.json";
const VERSION: u32 = 1;
/// While the limits stay the same, refresh `observedAt` at most this often:
/// readers learn the data is current without a write on every repaint.
const REFRESH: Duration = Duration::from_secs(60);
/// Codex writes a `token_count` event after every response; the newest one
/// sits near the end of the rollout.
const ROLLOUT_TAIL: u64 = 256 * 1024;
/// How long an account that hit its limit counts as full when the error does
/// not say until when: Claude Code's `StopFailure` input carries only the
/// error kind and its message. Past it, the 5-hour window has surely reset; a
/// spent weekly limit fails again and is learned again.
pub const LIMIT_HOLD: Duration = Duration::from_secs(5 * 3600);

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LimitWindow {
    /// `five_hour` or `seven_day`.
    pub name: String,
    #[serde(rename = "usedPercent")]
    pub used_percent: f64,
    /// Unix seconds.
    #[serde(rename = "resetsAt")]
    pub resets_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// Unix milliseconds.
    pub observed_at: u64,
    pub windows: Vec<LimitWindow>,
    /// Unix seconds until which a rate limit error counts the account as full.
    pub limited_until: Option<i64>,
    /// The plan the reading names (Codex only), such as `Pro`.
    pub plan: Option<String>,
    /// Unix seconds when the provider refused the login, until a reading works.
    pub login_refused_at: Option<i64>,
}

impl Reading {
    /// Whether a rate limit error still counts the account as full.
    pub fn limited(&self, now: SystemTime) -> bool {
        self.limited_until
            .is_some_and(|until| until > unix_secs(now))
    }

    /// Highest use across windows, counting a window that already reset as
    /// empty: an old reading still says when its limit came back.
    pub fn binding_used(&self, now: SystemTime) -> f64 {
        if self.limited(now) {
            return 100.0;
        }
        let now = unix_secs(now);
        self.windows
            .iter()
            .map(|w| {
                if w.resets_at <= now {
                    0.0
                } else {
                    w.used_percent
                }
            })
            .fold(0.0, f64::max)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct LimitsFile {
    v: u32,
    accounts: Vec<AccountLimits>,
}

#[derive(Serialize, Deserialize)]
struct AccountLimits {
    harness: Harness,
    account: String,
    label: String,
    #[serde(rename = "observedAt")]
    observed_at: u64,
    windows: Vec<LimitWindow>,
    #[serde(
        rename = "limitedUntil",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    limited_until: Option<i64>,
    #[serde(
        rename = "loginRefusedAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    login_refused_at: Option<i64>,
}

impl AccountLimits {
    fn new(account: &Account, id: &str, observed_at: u64, windows: Vec<LimitWindow>) -> Self {
        Self {
            harness: account.harness,
            account: id.to_owned(),
            label: account.label.clone(),
            observed_at,
            windows,
            limited_until: None,
            login_refused_at: None,
        }
    }
}

pub fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

pub fn unix_millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Claude readings by account id, from `limits.json`.
pub fn claude_readings(store: &Store) -> HashMap<String, Reading> {
    let Ok(bytes) = fs::read(store.state.join(FILE_NAME)) else {
        return HashMap::new();
    };
    let file: LimitsFile = serde_json::from_slice(&bytes).unwrap_or_default();
    file.accounts
        .into_iter()
        .filter(|a| a.harness == Harness::Claude)
        .map(|a| {
            let reading = Reading {
                observed_at: a.observed_at,
                windows: a.windows,
                limited_until: a.limited_until,
                plan: None,
                login_refused_at: a.login_refused_at,
            };
            (a.account, reading)
        })
        .collect()
}

/// Stores the reading of one account, which also says its login works.
/// Numbers that differ from the stored ones replace what a rate limit error
/// taught; the same numbers only say they are still current.
pub fn save(
    store: &Store,
    account: &Account,
    id: &str,
    windows: Vec<LimitWindow>,
    now: SystemTime,
) -> io::Result<()> {
    let observed_at = unix_millis(now);
    update(store, account.harness, id, |entry| match entry {
        Some(old) if old.windows == windows && old.label == account.label => {
            let refused = old.login_refused_at.take().is_some();
            if !refused && observed_at < old.observed_at + REFRESH.as_millis() as u64 {
                return false;
            }
            old.observed_at = observed_at;
            true
        }
        _ => {
            *entry = Some(AccountLimits::new(account, id, observed_at, windows));
            true
        }
    })
}

/// Stores a reading taken elsewhere at `observed_at` (Unix milliseconds),
/// unless the stored one is as new or newer. Returns whether it was stored.
pub fn seed(
    store: &Store,
    account: &Account,
    id: &str,
    observed_at: u64,
    windows: Vec<LimitWindow>,
) -> io::Result<bool> {
    let mut stored = false;
    update(store, account.harness, id, |entry| {
        if entry.as_ref().is_some_and(|e| e.observed_at >= observed_at) {
            return false;
        }
        *entry = Some(AccountLimits::new(account, id, observed_at, windows));
        stored = true;
        true
    })?;
    Ok(stored)
}

/// A turn of the account ended with a rate limit error: it counts as full
/// until its full window resets, when the last numbers say when, else for
/// `LIMIT_HOLD`.
pub fn mark_limited(store: &Store, account: &Account, id: &str, now: SystemTime) -> io::Result<()> {
    let now_secs = unix_secs(now);
    update(store, account.harness, id, |entry| {
        let entry = entry
            .get_or_insert_with(|| AccountLimits::new(account, id, unix_millis(now), Vec::new()));
        let full_until = entry
            .windows
            .iter()
            .filter(|w| w.used_percent >= 100.0 && w.resets_at > now_secs)
            .map(|w| w.resets_at)
            .max();
        entry.limited_until = Some(full_until.unwrap_or(now_secs + LIMIT_HOLD.as_secs() as i64));
        true
    })
}

/// The provider refused the account's login: it shows as expired, and auto
/// switching leaves it alone, until a reading works again. Keeps the first
/// refusal's time.
pub fn mark_refused(store: &Store, account: &Account, id: &str, now: SystemTime) -> io::Result<()> {
    update(store, account.harness, id, |entry| {
        let entry = entry
            .get_or_insert_with(|| AccountLimits::new(account, id, unix_millis(now), Vec::new()));
        if entry.login_refused_at.is_some() {
            return false;
        }
        entry.login_refused_at = Some(unix_secs(now));
        true
    })
}

/// The account signed in again: its login is no longer the refused one.
pub fn clear_refused(store: &Store, account: &Account, id: &str) -> io::Result<()> {
    update(store, account.harness, id, |entry| {
        entry
            .as_mut()
            .is_some_and(|e| e.login_refused_at.take().is_some())
    })
}

/// Read-modify-write of one account's entry. Sessions of different accounts
/// repaint at the same time, so it runs under a lock and the file is replaced
/// by rename; `change` returns whether there is anything to write.
fn update(
    store: &Store,
    harness: Harness,
    id: &str,
    change: impl FnOnce(&mut Option<AccountLimits>) -> bool,
) -> io::Result<()> {
    store.ensure_state()?;
    let path = store.state.join(FILE_NAME);
    let lock = open_private(&store.state.join(".limits.lock"), false)?;
    lock.lock()?;

    let mut file: LimitsFile = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => LimitsFile::default(),
    };
    let at = file
        .accounts
        .iter()
        .position(|a| a.harness == harness && a.account == id);
    let mut entry = at.map(|i| file.accounts.remove(i));
    if !change(&mut entry) {
        return Ok(());
    }
    if let Some(entry) = entry {
        file.accounts
            .insert(at.unwrap_or(file.accounts.len()), entry);
    }
    file.v = VERSION;

    let tmp = path.with_extension("json.tmp");
    let mut out = open_private(&tmp, true)?;
    out.write_all(&serde_json::to_vec(&file).expect("plain struct always serializes"))?;
    fs::rename(&tmp, &path)
}

fn open_private(path: &Path, truncate: bool) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)
}

#[derive(Deserialize)]
struct CodexEvent {
    payload: CodexPayload,
}

#[derive(Deserialize)]
struct CodexPayload {
    rate_limits: Option<CodexRateLimits>,
}

#[derive(Deserialize)]
struct CodexRateLimits {
    primary: Option<CodexWindow>,
    secondary: Option<CodexWindow>,
    plan_type: Option<String>,
}

#[derive(Deserialize)]
struct CodexWindow {
    used_percent: f64,
    window_minutes: u64,
    resets_at: i64,
}

/// The newest `rate_limits` in the account's newest rollout.
pub fn codex_reading(sessions: &Path) -> io::Result<Option<Reading>> {
    let Some(path) = newest_rollout(sessions)? else {
        return Ok(None);
    };
    let mut file = File::open(&path)?;
    let len = file.metadata()?.len();
    let observed_at = unix_millis(file.metadata()?.modified()?);
    file.seek(SeekFrom::Start(len.saturating_sub(ROLLOUT_TAIL)))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    for line in tail.rsplit(|b| *b == b'\n') {
        if !contains(line, b"\"rate_limits\"") {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<CodexEvent>(line) else {
            continue;
        };
        let Some(limits) = event.payload.rate_limits else {
            continue;
        };
        let plan = limits.plan_type.as_deref().map(crate::account::plan_name);
        let windows = [limits.primary, limits.secondary]
            .into_iter()
            .flatten()
            .map(|w| LimitWindow {
                name: window_name(w.window_minutes),
                used_percent: w.used_percent,
                resets_at: w.resets_at,
            })
            .collect();
        return Ok(Some(Reading {
            observed_at,
            windows,
            limited_until: None,
            plan,
            login_refused_at: None,
        }));
    }
    Ok(None)
}

fn window_name(minutes: u64) -> String {
    match minutes {
        300 => "five_hour".to_owned(),
        10_080 => "seven_day".to_owned(),
        other => format!("{other}_minutes"),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
