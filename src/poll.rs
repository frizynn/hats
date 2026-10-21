//! Each Claude Code account's plan limits from the provider, the way Claude
//! Code's `/usage` and claude-swap read them: `GET /api/oauth/usage` with the
//! account's own token. Without it an account shows only what its sessions
//! last reported, which can be days old.
//!
//! The token of the login in the seat is Claude Code's to refresh; it is used
//! only while valid. A parked login with an expired token is refreshed here,
//! under the same locks Claude Code takes for that directory, and saved back
//! at once: the provider rotates the refresh token.
//!
//! Requests go through `curl` with the headers on its standard input, so no
//! token shows in the process list.

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;

use crate::account::{Account, Harness};
use crate::config::HarnessConfig;
use crate::limits::{self, LimitWindow, unix_millis};
use crate::seat::{self, Lock};
use crate::store::Store;
use crate::{auto, secret};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code's own OAuth client.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA: &str = "oauth-2025-04-20";
/// A token this close to expiring counts as expired.
const EXPIRY_MARGIN_MS: u64 = 5 * 60 * 1000;
/// How often the limits are asked for at most.
pub const EVERY: Duration = Duration::from_secs(60);
const POLLED: &str = "polled";

/// Whether the last poll is older than `EVERY`. Never with `HATS_OFFLINE`
/// set, which keeps hats off the network.
pub fn due(store: &Store, now: SystemTime) -> bool {
    if offline() {
        return false;
    }
    std::fs::metadata(store.state.join(POLLED))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| now.duration_since(at).ok())
        .is_none_or(|age| age >= EVERY)
}

pub fn offline() -> bool {
    std::env::var_os("HATS_OFFLINE").is_some_and(|v| !v.is_empty())
}

/// Asks for every signed-in account's limits at once, stores them, and lets
/// auto switching act on them. Returns the accounts that could not be read,
/// with why.
pub fn poll(store: &Store, config: &HarnessConfig, now: SystemTime) -> Vec<(String, String)> {
    if offline() {
        return Vec::new();
    }
    let _ = store
        .ensure_state()
        .and_then(|()| std::fs::write(store.state.join(POLLED), b""));
    let seat_id = seat::seat(store).provider_id(store).ok().flatten();
    let accounts: Vec<(Account, String)> = Account::all(Harness::Claude, store)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| {
            let id = a.provider_id(store).ok().flatten()?;
            // The seat itself only while no account holds its login.
            (!a.is_default || seat::holder(store, &id).is_none()).then_some((a, id))
        })
        .collect();
    let results: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = accounts
            .iter()
            .map(|(account, id)| {
                let seated = seat_id.as_deref() == Some(id.as_str());
                scope.spawn(move || read_one(store, account, id, seated, now))
            })
            .collect();
        handles
            .into_iter()
            .zip(&accounts)
            .map(|(h, (account, _))| {
                let result = h.join().unwrap_or_else(|_| Err("crashed".to_owned()));
                (account.label.clone(), result)
            })
            .collect()
    });
    let _ = auto::check_default(store, Harness::Claude, config, now, None, None);
    results
        .into_iter()
        .filter_map(|(label, r)| r.err().map(|e| (label, e)))
        .collect()
}

fn read_one(
    store: &Store,
    account: &Account,
    id: &str,
    seated: bool,
    now: SystemTime,
) -> Result<(), String> {
    // The seat holds the live login of the account sitting in it; its own
    // directory has an older copy.
    let source = if seated {
        seat::seat(store)
    } else {
        account.clone()
    };
    let mut login = secret::login(store, &source)
        .map_err(|e| e.to_string())?
        .ok_or("no stored login")?;
    if expired(&login, now) {
        if seated {
            return Err("its token expired; Claude Code renews it on its next request".to_owned());
        }
        login = renew(store, account, now)?;
    }
    let token = login["accessToken"].as_str().ok_or("no access token")?;
    let windows = usage(token)?;
    limits::save(store, account, id, windows, now).map_err(|e| e.to_string())
}

fn expired(login: &Value, now: SystemTime) -> bool {
    login["expiresAt"]
        .as_u64()
        .is_none_or(|at| at <= unix_millis(now) + EXPIRY_MARGIN_MS)
}

#[derive(Deserialize)]
struct Usage {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

fn usage(token: &str) -> Result<Vec<LimitWindow>, String> {
    let (status, body) = curl(&format!(
        "url = \"{USAGE_URL}\"\nheader = \"Authorization: Bearer {token}\"\nheader = \"anthropic-beta: {BETA}\"\n"
    ))?;
    if status == 401 {
        return Err("the provider refused its login; sign in again with `hats login`".to_owned());
    }
    if status != 200 {
        return Err(format!("the provider answered {status}"));
    }
    let usage: Usage = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok([
        ("five_hour", usage.five_hour),
        ("seven_day", usage.seven_day),
    ]
    .into_iter()
    .filter_map(|(name, w)| {
        let w = w?;
        Some(LimitWindow {
            name: name.to_owned(),
            used_percent: w.utilization?,
            resets_at: w.resets_at.as_deref().and_then(parse_time).unwrap_or(0),
        })
    })
    .collect())
}

/// A new token for a parked login, saved back before it is used. Under the
/// directory's refresh locks the login is read again: another refresh may
/// have run meanwhile.
fn renew(store: &Store, account: &Account, now: SystemTime) -> Result<Value, String> {
    let _refresh = Lock::take(account.dir.join(".oauth_refresh.lock"), seat::REFRESH_STALE)?;
    let _legacy = Lock::take(
        seat::with_suffix(&account.dir, ".lock"),
        seat::REFRESH_STALE,
    )?;
    // It may have taken the seat while this waited: a renewal now would
    // rotate the refresh token the seat's copy holds.
    if seat::current(store).label == account.label {
        return Err("its token expired; Claude Code renews it on its next request".to_owned());
    }
    let mut login = secret::login(store, account)
        .map_err(|e| e.to_string())?
        .ok_or("no stored login")?;
    if !expired(&login, now) {
        return Ok(login);
    }
    let refresh = login["refreshToken"].as_str().ok_or("no refresh token")?;
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "client_id": CLIENT_ID,
    })
    .to_string();
    let (status, answer) = curl(&format!(
        "url = \"{TOKEN_URL}\"\nheader = \"Content-Type: application/json\"\ndata-binary = \"{}\"\n",
        body.replace('\\', "\\\\").replace('"', "\\\"")
    ))?;
    if status != 200 {
        return Err(if (400..500).contains(&status) {
            "its login expired; sign in again with `hats login`".to_owned()
        } else {
            format!("the provider answered {status} to renew its token")
        });
    }
    let answer: Value = serde_json::from_slice(&answer).map_err(|e| e.to_string())?;
    let access = answer["access_token"].as_str().ok_or("no access token")?;
    let expires_in = answer["expires_in"].as_u64().ok_or("no expiry")?;
    login["accessToken"] = access.into();
    login["expiresAt"] = (unix_millis(SystemTime::now()) + expires_in * 1000).into();
    if let Some(next) = answer["refresh_token"].as_str() {
        login["refreshToken"] = next.into();
    }
    if let Some(scope) = answer["scope"].as_str() {
        login["scopes"] = scope.split_whitespace().collect::<Vec<_>>().into();
    }
    secret::put_login(store, account, login.clone())
        .map_err(|e| format!("renewed its token but could not save it: {e}"))?;
    Ok(login)
}

/// Runs `curl` with `config` on its standard input; returns the status and
/// the body.
fn curl(config: &str) -> Result<(u16, Vec<u8>), String> {
    let mut child = Command::new("curl")
        .args(["-sS", "--max-time", "8", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start curl: {e}"))?;
    let config = format!(
        "{config}header = \"User-Agent: hats/{}\"\nwrite-out = \"\\n%{{http_code}}\"\n",
        env!("CARGO_PKG_VERSION")
    );
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(config.as_bytes())
        .map_err(|e: io::Error| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(format!("no answer: {}", why.trim()));
    }
    let mut body = out.stdout;
    let at = body.iter().rposition(|b| *b == b'\n').ok_or("no status")?;
    let status = std::str::from_utf8(&body[at + 1..])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or("no status")?;
    body.truncate(at);
    Ok((status, body))
}

/// Unix seconds of an RFC 3339 time such as `2026-10-20T03:49:59.73+00:00`.
fn parse_time(text: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let zone = text
        .get(19..)?
        .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone {
        "Z" | "" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let h: i64 = zone.get(1..3)?.parse().ok()?;
            let m: i64 = zone.get(4..6)?.parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_providers_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_time("2026-10-20T03:49:59.730532+00:00"),
            Some(1_792_468_199)
        );
        assert_eq!(parse_time("2026-10-20T05:49:59+02:00"), Some(1_792_468_199));
        assert_eq!(parse_time("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(parse_time("soon"), None);
    }
}
