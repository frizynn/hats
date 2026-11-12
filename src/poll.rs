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
use crate::limits::{self, Gift, LimitWindow, unix_millis};
use crate::seat::{self, Lock};
use crate::store::Store;
use crate::{auto, secret};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code's own OAuth client.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA: &str = "oauth-2025-04-20";
/// The provider lists an account's gifted limit resets only to Claude Code's
/// command line (measured with Claude Code 2.1.322: any other agent gets
/// `"ineligible_reason": "surface"`).
const CLI_AGENT: &str = "claude-cli/2.1.322 (external, cli)";
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

/// Makes the next `due` true: a new login wants its limits read at once.
pub fn ask_soon(store: &Store) {
    let _ = std::fs::remove_file(store.state.join(POLLED));
}

pub fn offline() -> bool {
    std::env::var_os("HATS_OFFLINE").is_some_and(|v| !v.is_empty())
}

/// Why an account's limits could not be read.
enum Failed {
    /// The provider refused the login: it expired or was revoked, and only
    /// signing in again helps. Kept with the account's limits.
    Refused(String),
    /// Anything else (network, timeouts, the provider busy): the next poll
    /// tries again, and nothing is kept.
    Other(String),
}

impl From<String> for Failed {
    fn from(why: String) -> Self {
        Self::Other(why)
    }
}

impl From<&str> for Failed {
    fn from(why: &str) -> Self {
        Self::Other(why.to_owned())
    }
}

/// Asks for every signed-in account's limits at once, stores them, and lets
/// auto switching act on them. Returns the accounts that could not be read,
/// with why. A refused login is kept with the account's limits (see
/// `limits::mark_refused`).
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
                scope.spawn(move || {
                    let result = read_one(store, account, id, seated, now);
                    if let Err(Failed::Refused(_)) = &result {
                        let _ = limits::mark_refused(store, account, id, now);
                    }
                    result
                })
            })
            .collect();
        handles
            .into_iter()
            .zip(&accounts)
            .map(|(h, (account, _))| {
                let result = h.join().unwrap_or_else(|_| Err("crashed".into()));
                (account.label.clone(), result)
            })
            .collect()
    });
    let _ = auto::check_default(store, Harness::Claude, config, now, None, None);
    results
        .into_iter()
        .filter_map(|(label, r)| match r {
            Ok(()) => None,
            Err(Failed::Refused(why) | Failed::Other(why)) => Some((label, why)),
        })
        .collect()
}

fn read_one(
    store: &Store,
    account: &Account,
    id: &str,
    seated: bool,
    now: SystemTime,
) -> Result<(), Failed> {
    // The seat holds the live login of the account sitting in it; its own
    // directory has an older copy.
    let source = if seated {
        seat::seat(store)
    } else {
        account.clone()
    };
    let read = secret::login(store, &source)
        .map_err(|e| e.to_string())?
        .ok_or("no stored login")?;
    match read_with(store, account, id, seated, now, read.clone()) {
        // A new login landed meanwhile (`hats login`): the refusal was the
        // old one's, and the next poll reads the new one.
        Err(Failed::Refused(_))
            if secret::login(store, &source).ok().flatten().as_ref() != Some(&read) =>
        {
            Err("signed in again while its old login was read".into())
        }
        result => result,
    }
}

/// Reads the limits with `login`, renewing it first when it expired.
fn read_with(
    store: &Store,
    account: &Account,
    id: &str,
    seated: bool,
    now: SystemTime,
    mut login: Value,
) -> Result<(), Failed> {
    if expired(&login, now) {
        if seated {
            return Err("its token expired; Claude Code renews it on its next request".into());
        }
        login = renew(store, account, now)?;
    }
    let token = login["accessToken"].as_str().ok_or("no access token")?;
    let (windows, gifts) = usage(token)?;
    limits::save(store, account, id, windows, gifts, now).map_err(|e| e.to_string().into())
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
    /// Every limit, among them the weekly limit of one model.
    #[serde(default)]
    limits: Vec<UsageLimit>,
    /// The limit resets given to the account.
    cedar_ember: Option<Gifts>,
}

#[derive(Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct UsageLimit {
    kind: String,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<Value>,
}

#[derive(Deserialize)]
struct Gifts {
    #[serde(default)]
    grants: Vec<Grant>,
}

#[derive(Deserialize)]
struct Grant {
    label: Option<String>,
    resets_total: Option<u32>,
    resets_left: Option<u32>,
    ends_at: Option<String>,
    #[serde(default)]
    clears: Vec<String>,
    #[serde(default)]
    usable_now: bool,
}

/// The windows, and the gifts when the provider listed them.
fn usage(token: &str) -> Result<(Vec<LimitWindow>, Option<Vec<Gift>>), Failed> {
    let (status, body) = curl(&format!(
        "url = \"{USAGE_URL}?cedar_ember=1\"\nheader = \"Authorization: Bearer {token}\"\nheader = \"anthropic-beta: {BETA}\"\nheader = \"User-Agent: {CLI_AGENT}\"\n"
    ))?;
    if status == 401 {
        return Err(Failed::Refused(
            "the provider refused its login; sign in again with `hats login`".to_owned(),
        ));
    }
    if status != 200 {
        return Err(format!("the provider answered {status}").into());
    }
    let usage: Usage = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok(read_usage(usage))
}

fn read_usage(usage: Usage) -> (Vec<LimitWindow>, Option<Vec<Gift>>) {
    let mut windows: Vec<LimitWindow> = [
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
    .collect();
    windows.extend(usage.limits.into_iter().filter_map(|l| {
        let model = l.scope.as_ref()?["model"]["display_name"]
            .as_str()?
            .to_lowercase();
        (l.kind == "weekly_scoped" && !model.is_empty()).then(|| LimitWindow {
            name: format!("seven_day_{model}"),
            used_percent: l.percent.unwrap_or(0.0),
            resets_at: l.resets_at.as_deref().and_then(parse_time).unwrap_or(0),
        })
    }));
    let gifts = usage.cedar_ember.map(|g| {
        g.grants
            .into_iter()
            .map(|g| Gift {
                label: g.label.unwrap_or_default(),
                left: g.resets_left.unwrap_or(0),
                total: g.resets_total.unwrap_or(0),
                ends_at: g.ends_at.as_deref().and_then(parse_time).unwrap_or(0),
                clears: g.clears,
                usable_now: g.usable_now,
            })
            .collect()
    });
    (windows, gifts)
}

/// A new token for a parked login, saved back before it is used. Under the
/// directory's refresh locks the login is read again: another refresh may
/// have run meanwhile.
fn renew(store: &Store, account: &Account, now: SystemTime) -> Result<Value, Failed> {
    let _refresh = Lock::take(account.dir.join(".oauth_refresh.lock"), seat::REFRESH_STALE)?;
    let _legacy = Lock::take(
        seat::with_suffix(&account.dir, ".lock"),
        seat::REFRESH_STALE,
    )?;
    // It may have taken the seat while this waited: a renewal now would
    // rotate the refresh token the seat's copy holds.
    if seat::current(store).label == account.label {
        return Err("its token expired; Claude Code renews it on its next request".into());
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
    // OAuth answers an expired or revoked refresh token with 400
    // (`invalid_grant`); 429 and the 5xx pass.
    if matches!(status, 400 | 401) {
        return Err(Failed::Refused(
            "its login expired; sign in again with `hats login`".to_owned(),
        ));
    }
    if status != 200 {
        return Err(format!("the provider answered {status} to renew its token").into());
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
    let agent = if config.contains("User-Agent:") {
        String::new()
    } else {
        format!(
            "header = \"User-Agent: hats/{}\"\n",
            env!("CARGO_PKG_VERSION")
        )
    };
    let config = format!("{config}{agent}write-out = \"\\n%{{http_code}}\"\n");
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
    fn reads_a_model_limit_and_the_gifts() {
        // Cut from a real answer, 2026-11-12.
        let body = r#"{"five_hour":{"utilization":0.0,"resets_at":null},
            "seven_day":{"utilization":60.0,"resets_at":"2026-11-16T08:59:59.912030+00:00"},
            "limits":[{"kind":"session","percent":0,"resets_at":null,"scope":null},
              {"kind":"weekly_all","percent":60,"resets_at":"2026-11-16T08:59:59+00:00","scope":null},
              {"kind":"weekly_scoped","percent":51,"resets_at":"2026-11-16T08:59:59+00:00",
               "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null}}],
            "cedar_ember":{"eligible":true,"grants":[{"id":"full-reset-2611",
              "label":"One full limit reset (November 2026)","resets_total":1,"resets_left":1,
              "ends_at":"2026-12-10T22:00:00+00:00","clears":["five_hour","seven_day"],
              "usable_now":true}]}}"#;
        let (windows, gifts) = read_usage(serde_json::from_str(body).unwrap());
        let fable = windows
            .iter()
            .find(|w| w.name == "seven_day_fable")
            .unwrap();
        assert_eq!(fable.used_percent, 51.0);
        let gifts = gifts.unwrap();
        assert_eq!(gifts.len(), 1);
        assert!(gifts[0].clears_weekly() && gifts[0].left == 1);
        assert_eq!(
            gifts[0].ends_at,
            parse_time("2026-12-10T22:00:00Z").unwrap()
        );
        // Without the gifts block, the stored gifts stay.
        let (_, gifts) = read_usage(serde_json::from_str(r#"{"five_hour":null}"#).unwrap());
        assert!(gifts.is_none());
    }

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
