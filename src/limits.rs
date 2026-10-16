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
}

impl Reading {
    /// Highest use across windows, counting a window that already reset as
    /// empty: an old reading still says when its limit came back.
    pub fn binding_used(&self, now: SystemTime) -> f64 {
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
            };
            (a.account, reading)
        })
        .collect()
}

/// Stores the reading of one account. Sessions of different accounts repaint
/// at the same time, so the read-modify-write runs under a lock and the file
/// is replaced by rename.
pub fn save(
    store: &Store,
    account: &Account,
    id: &str,
    windows: Vec<LimitWindow>,
    now: SystemTime,
) -> io::Result<()> {
    store.ensure_state()?;
    let path = store.state.join(FILE_NAME);
    let lock = open_private(&store.state.join(".limits.lock"), false)?;
    lock.lock()?;

    let mut file: LimitsFile = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => LimitsFile::default(),
    };
    let observed_at = unix_millis(now);
    let fresh = AccountLimits {
        harness: account.harness,
        account: id.to_owned(),
        label: account.label.clone(),
        observed_at,
        windows,
    };
    let existing = file
        .accounts
        .iter_mut()
        .find(|a| a.harness == fresh.harness && a.account == fresh.account);
    match existing {
        Some(old) => {
            let unchanged = old.windows == fresh.windows && old.label == fresh.label;
            if unchanged && observed_at < old.observed_at + REFRESH.as_millis() as u64 {
                return Ok(());
            }
            *old = fresh;
        }
        None => file.accounts.push(fresh),
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
