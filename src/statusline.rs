//! Claude Code status line wrapper. Claude Code hands the status line command
//! the session's plan limits (`rate_limits`) on every repaint; the wrapper
//! keeps them per account, applies the auto policy, and runs the user's own
//! status line command with the same input.

use std::ffi::OsString;
use std::path::Path;
use std::time::SystemTime;

use serde::Deserialize;

use crate::account::{Account, Harness};
use crate::config::HarnessConfig;
use crate::limits::{self, LimitWindow};
use crate::store::Store;
use crate::{auto, supervisor};

#[derive(Deserialize)]
struct StatusInput {
    session_id: Option<String>,
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

/// Records the limits in `input` for the session's account and returns a
/// one-line summary such as `work 5h 24% 7d 41%`.
pub fn record(
    store: &Store,
    config: &HarnessConfig,
    config_dir: Option<OsString>,
    input: &[u8],
    now: SystemTime,
) -> (String, Result<(), String>) {
    let config_dir = config_dir.filter(|dir| !dir.is_empty());
    let account =
        Account::from_config_dir(Harness::Claude, config_dir.as_deref().map(Path::new), store);
    let input: Option<StatusInput> = serde_json::from_slice(input).ok();
    let windows = input.as_ref().map(parse_windows).unwrap_or_default();
    let mut summary = account.label.clone();
    for w in &windows {
        let short = if w.name == "five_hour" { "5h" } else { "7d" };
        summary.push_str(&format!(" {short} {:.0}%", w.used_percent));
    }
    let session = input.and_then(|i| i.session_id);
    let result = if windows.is_empty() {
        Ok(())
    } else {
        apply(store, config, &account, session.as_deref(), windows, now)
    };
    (summary, result)
}

fn parse_windows(input: &StatusInput) -> Vec<LimitWindow> {
    let Some(limits) = &input.rate_limits else {
        return Vec::new();
    };
    [
        ("five_hour", &limits.five_hour),
        ("seven_day", &limits.seven_day),
    ]
    .into_iter()
    .filter_map(|(name, w)| {
        w.as_ref().map(|w| LimitWindow {
            name: name.to_owned(),
            used_percent: w.used_percentage,
            resets_at: w.resets_at,
        })
    })
    .collect()
}

fn apply(
    store: &Store,
    config: &HarnessConfig,
    account: &Account,
    session: Option<&str>,
    windows: Vec<LimitWindow>,
    now: SystemTime,
) -> Result<(), String> {
    let Some(id) = account.provider_id(store).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    limits::save(store, account, &id, windows, now).map_err(|e| e.to_string())?;
    auto::check_default(
        store,
        Harness::Claude,
        config,
        now,
        None,
        Some(&account.label),
    )?;
    if let Some(session) = session {
        supervisor::request_move(store, config, account, session, now)?;
    }
    Ok(())
}
