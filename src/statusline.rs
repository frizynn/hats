//! Claude Code status line wrapper. Claude Code gives the status line command
//! the session's plan limits (`rate_limits`); this keeps the last ones seen per
//! account in `limits.json` so they can be shown for accounts that are not
//! running. Only data Claude Code hands to the status line: no credentials,
//! no requests.
//!
//! `limits.json`, version 1:
//!
//! ```json
//! {"v":1,"accounts":[{"harness":"claude","account":"0123456789abcdef","label":"work","observedAt":1767323045678,"windows":[{"name":"five_hour","usedPercent":23.5,"resetsAt":1767330000}]}]}
//! ```

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::account::{Account, Harness};
use crate::store::Store;

pub const FILE_NAME: &str = "limits.json";
const VERSION: u32 = 1;
/// While the limits stay the same, refresh `observedAt` at most this often:
/// readers learn the data is current without a write on every repaint.
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct StatusInput {
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct RateLimits {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    used_percentage: f64,
    resets_at: i64,
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

#[derive(Serialize, Deserialize, PartialEq)]
struct LimitWindow {
    name: String,
    #[serde(rename = "usedPercent")]
    used_percent: f64,
    #[serde(rename = "resetsAt")]
    resets_at: i64,
}

/// Records the limits in `input` for the account in `config_dir` and returns
/// a one-line summary such as `work 5h 24% 7d 41%`.
pub fn record(
    store: &Store,
    config_dir: Option<OsString>,
    input: &[u8],
    now: SystemTime,
) -> (String, Result<(), String>) {
    let config_dir = config_dir.filter(|dir| !dir.is_empty());
    let account =
        Account::from_config_dir(Harness::Claude, config_dir.as_deref().map(Path::new), store);
    let windows = parse_windows(input);
    let mut summary = account.label.clone();
    for w in &windows {
        let short = if w.name == "five_hour" { "5h" } else { "7d" };
        summary.push_str(&format!(" {short} {:.0}%", w.used_percent));
    }
    let saved = if windows.is_empty() {
        Ok(())
    } else {
        save(store, &account, windows, now)
    };
    (summary, saved)
}

fn parse_windows(input: &[u8]) -> Vec<LimitWindow> {
    let Ok(StatusInput {
        rate_limits: Some(limits),
    }) = serde_json::from_slice(input)
    else {
        return Vec::new();
    };
    [
        ("five_hour", limits.five_hour),
        ("seven_day", limits.seven_day),
    ]
    .into_iter()
    .filter_map(|(name, w)| {
        w.map(|w| LimitWindow {
            name: name.to_owned(),
            used_percent: w.used_percentage,
            resets_at: w.resets_at,
        })
    })
    .collect()
}

fn save(
    store: &Store,
    account: &Account,
    windows: Vec<LimitWindow>,
    now: SystemTime,
) -> Result<(), String> {
    let Some(id) = account.provider_id(store).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    store.ensure_state().map_err(|e| e.to_string())?;
    let path = store.state.join(FILE_NAME);
    // Sessions of different accounts repaint at the same time: serialize the
    // read-modify-write so no account's entry is lost.
    let lock = open_private(&store.state.join(".limits.lock"), false).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;

    let mut file: LimitsFile = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => LimitsFile::default(),
    };
    let observed_at = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let fresh = AccountLimits {
        harness: account.harness,
        account: id,
        label: account.label.clone(),
        observed_at,
        windows,
    };
    match file
        .accounts
        .iter_mut()
        .find(|a| a.harness == fresh.harness && a.account == fresh.account)
    {
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
    let mut out = open_private(&tmp, true).map_err(|e| e.to_string())?;
    out.write_all(&serde_json::to_vec(&file).expect("plain struct always serializes"))
        .and_then(|()| fs::rename(&tmp, &path))
        .map_err(|e| e.to_string())
}

fn open_private(path: &Path, truncate: bool) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .mode(0o600)
        .open(path)
}
