//! `hats import claude-swap`: takes claude-swap's account order, aliases,
//! switching settings, last limits and each account's login. It reads
//! `sequence.json`, `settings.json`, the identity in claude-swap's copies of
//! each account's `.claude.json`, the percentages in `cache/usage.json` and
//! claude-swap's backup of each login: `credentials/.creds-<n>-<mail>.enc`
//! (base64), or on macOS, when that file is absent or unreadable as base64,
//! the Keychain item `claude-swap` / `account-<n>-<mail>` (claude-swap 0.26).
//! The login goes where Claude Code reads it (see `secret`); a directory that
//! already holds one keeps it unless asked to replace it.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::account::{self, Account, DEFAULT_LABEL, Harness, validate_label};
use crate::config;
use crate::limits::{self, LimitWindow};
use crate::registry;
use crate::seat::{self, Lock};
use crate::secret;
use crate::store::Store;

/// claude-swap's Keychain service for its backups of each account's login.
const KEYCHAIN_SERVICE: &str = "claude-swap";

#[derive(Deserialize)]
struct Sequence {
    #[serde(default)]
    accounts: BTreeMap<String, SwapAccount>,
    #[serde(default)]
    sequence: Vec<u32>,
}

#[derive(Deserialize)]
struct SwapAccount {
    #[serde(default)]
    email: String,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize, Default)]
struct SwapSettings {
    #[serde(default)]
    autoswitch: SwapAutoswitch,
}

#[derive(Deserialize, Default)]
struct SwapAutoswitch {
    threshold: Option<f64>,
    strategy: Option<String>,
}

/// `cache/usage.json`: claude-swap's last good reading of each account, by
/// account number. Only the percentages and times are read.
#[derive(Deserialize, Default)]
struct SwapUsage {
    #[serde(default)]
    accounts: BTreeMap<String, SwapSlot>,
}

#[derive(Deserialize)]
struct SwapSlot {
    /// Unix seconds.
    #[serde(rename = "fetchedAt")]
    fetched_at: Option<f64>,
    #[serde(rename = "lastGood")]
    last_good: Option<SwapGood>,
}

#[derive(Deserialize)]
struct SwapGood {
    five_hour: Option<SwapWindow>,
    seven_day: Option<SwapWindow>,
}

#[derive(Deserialize)]
struct SwapWindow {
    pct: f64,
    resets_at: String,
}

impl SwapSlot {
    /// The reading as hats keeps it: when it was taken, in milliseconds, and
    /// the windows whose reset time reads.
    fn reading(&self) -> Option<(u64, Vec<LimitWindow>)> {
        let fetched_at = self.fetched_at.filter(|s| s.is_finite() && *s > 0.0)?;
        let good = self.last_good.as_ref()?;
        let windows: Vec<LimitWindow> = [
            ("five_hour", &good.five_hour),
            ("seven_day", &good.seven_day),
        ]
        .into_iter()
        .filter_map(|(name, w)| {
            let w = w.as_ref()?;
            Some(LimitWindow {
                name: name.to_owned(),
                used_percent: w.pct,
                resets_at: registry::parse_utc(&w.resets_at)?,
            })
        })
        .collect();
        (!windows.is_empty()).then_some(((fetched_at * 1000.0) as u64, windows))
    }
}

/// Where claude-swap keeps its data: `$XDG_DATA_HOME/claude-swap` on Linux
/// (`~/.local/share/claude-swap` when unset or relative),
/// `~/.claude-swap-backup` elsewhere.
pub fn claude_swap_dir(store: &Store) -> PathBuf {
    if cfg!(target_os = "macos") {
        return store.home.join(".claude-swap-backup");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| store.home.join(".local/share"))
        .join("claude-swap")
}

pub fn claude_swap(store: &Store, from: &Path, replace_logins: bool) -> Result<(), String> {
    let read = |name: &str| {
        fs::read(from.join(name)).map_err(|e| format!("{}: {e}", from.join(name).display()))
    };
    let sequence: Sequence = serde_json::from_slice(&read("sequence.json")?)
        .map_err(|e| format!("sequence.json: {e}"))?;
    let settings: SwapSettings = match read("settings.json") {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("settings.json: {e}"))?,
        Err(_) => SwapSettings::default(),
    };
    // Optional: without it the accounts start with no limits seen.
    let usage: SwapUsage = read("cache/usage.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut seeded = Vec::new();
    let mut seed = |account: &Account, id: &str, n: u32| -> Result<(), String> {
        let Some((observed_at, windows)) = usage
            .accounts
            .get(&n.to_string())
            .and_then(SwapSlot::reading)
        else {
            return Ok(());
        };
        if limits::seed(store, account, id, observed_at, windows).map_err(|e| e.to_string())? {
            seeded.push(account.label.clone());
        }
        Ok(())
    };

    // claude-swap's rotation order, then any account it lists but not in order.
    let mut numbers: Vec<u32> = sequence.sequence.clone();
    for key in sequence.accounts.keys() {
        if let Ok(n) = key.parse::<u32>()
            && !numbers.contains(&n)
        {
            numbers.push(n);
        }
    }

    // The login already in ~/.claude is one of claude-swap's accounts: it
    // stays `default` instead of signing in to it a second time.
    let mut signed_in = Account::default_for(Harness::Claude, store)
        .provider_id(store)
        .ok()
        .flatten();
    let mut order = Vec::new();
    let mut imported = Vec::new();
    let mut kept_logins = Vec::new();
    let mut to_sign_in = Vec::new();
    let mut skipped = Vec::new();
    for n in numbers {
        let Some(swap) = sequence.accounts.get(&n.to_string()) else {
            continue;
        };
        let label = swap
            .alias
            .as_deref()
            .filter(|a| validate_label(a).is_ok())
            .map_or_else(|| format!("account-{n}"), str::to_owned);
        if swap.disabled {
            skipped.push(format!("{label} ({})", swap.email));
            continue;
        }
        let id = swap_account_id(from, n, &swap.email);
        if signed_in.is_some() && id == signed_in {
            println!(
                "Kept    {DEFAULT_LABEL}: claude-swap's {label} ({}) is already signed in at ~/.claude",
                swap.email
            );
            let home = Account::default_for(Harness::Claude, store);
            seed(&home, id.as_deref().expect("equal to signed_in"), n)?;
            order.push(DEFAULT_LABEL.to_owned());
            signed_in = None;
            continue;
        }
        let account = Account::managed(Harness::Claude, &label, store)?;
        let created = !account.dir.exists();
        account.create(store).map_err(|e| e.to_string())?;
        // Keyed by the account id, so the numbers apply once this directory
        // signs in to that same account.
        if let Some(id) = &id {
            seed(&account, id, n)?;
        }
        // A login that cannot be read is never replaced unasked.
        let has_login = !matches!(secret::login(store, &account), Ok(None));
        if has_login && !replace_logins {
            kept_logins.push(label.clone());
        } else {
            let keychain = |user: &str| secret::keychain_password(store, KEYCHAIN_SERVICE, user);
            match import_login(store, &account, from, n, &swap.email, &keychain) {
                Ok(()) => imported.push(label.clone()),
                Err(why) if has_login => {
                    println!("Kept    {label}'s login: {why}");
                    kept_logins.push(label.clone());
                }
                Err(why) => to_sign_in.push((label.clone(), swap.email.clone(), why)),
            }
        }
        println!("{} {label}", if created { "Created" } else { "Kept   " });
        order.push(label);
    }

    if !order.is_empty() {
        config::set(&store.config, "claude.order", &order.join(","))?;
        println!("claude.order = {}", order.join(","));
    }
    if let Some(threshold) = settings.autoswitch.threshold {
        let threshold = threshold.round().clamp(1.0, 100.0) as u8;
        config::set(&store.config, "claude.threshold", &threshold.to_string())?;
        println!("claude.threshold = {threshold}");
    }
    match settings.autoswitch.strategy.as_deref() {
        Some("best") => {
            config::set(&store.config, "claude.strategy", "headroom")?;
            println!("claude.strategy = headroom (claude-swap: best)");
        }
        Some(other) => println!(
            "claude-swap strategy '{other}' has no equivalent; kept claude.strategy as it was"
        ),
        None => {}
    }
    if !seeded.is_empty() {
        println!(
            "Limits  from claude-swap's last reading: {}",
            seeded.join(", ")
        );
    }
    for s in &skipped {
        println!("Skipped {s}: disabled in claude-swap");
    }
    if !imported.is_empty() {
        println!("Logins  from claude-swap: {}", imported.join(", "));
    }
    if !kept_logins.is_empty() {
        println!(
            "Kept    the logins already in {} (`--replace-logins` takes claude-swap's instead)",
            kept_logins.join(", ")
        );
    }
    if !to_sign_in.is_empty() {
        println!(
            "\nSign in to each account with Claude Code's own login (rename first with `hats rm` and `hats add` if you want another label):"
        );
        for (label, email, why) in &to_sign_in {
            println!("  hats login {label}    # {email}: {why}");
        }
    }
    if !imported.is_empty() {
        println!(
            "\nhats and claude-swap now hold the same refresh token for {}: when one renews it, the other's copy stops working. Stop `cswap auto` now, and do not switch with cswap again.",
            imported.join(", ")
        );
    }
    println!(
        "\nWhen every account works in hats, stop `cswap auto` and run `cswap purge` to delete claude-swap's stored tokens."
    );
    Ok(())
}

/// Copies claude-swap's backup of account `number` into `account`: its
/// `claudeAiOauth` where Claude Code reads the login, its `oauthAccount` in
/// the directory's `.claude.json`. Other keys of both stay. The error says
/// why, never with a token in it.
fn import_login(
    store: &Store,
    account: &Account,
    from: &Path,
    number: u32,
    email: &str,
    keychain: &dyn Fn(&str) -> io::Result<Option<String>>,
) -> Result<(), String> {
    let login = swap_login(from, number, email, keychain)?
        .ok_or_else(|| "claude-swap has no login stored for it".to_owned())?;
    let identity = seat::oauth_account(&swap_config(from, number, email))
        .map_err(|_| "claude-swap has no account details for it".to_owned())?;
    // A renewal in flight (see `poll`) must not land over it.
    let _refresh = Lock::take(account.dir.join(".oauth_refresh.lock"), seat::REFRESH_STALE)?;
    let _legacy = Lock::take(
        seat::with_suffix(&account.dir, ".lock"),
        seat::REFRESH_STALE,
    )?;
    secret::put_login(store, account, login).map_err(|e| e.to_string())?;
    seat::set_oauth_account(&account.dir.join(".claude.json"), identity).map_err(|e| e.to_string())
}

/// The `claudeAiOauth` of claude-swap's backup of one account, read as
/// claude-swap reads it: the `.enc` file wins, and only when it is absent,
/// empty or not base64 does the Keychain answer. An API key (no
/// `claudeAiOauth`) reads as no login.
fn swap_login(
    from: &Path,
    number: u32,
    email: &str,
    keychain: &dyn Fn(&str) -> io::Result<Option<String>>,
) -> Result<Option<serde_json::Value>, String> {
    let enc = from
        .join("credentials")
        .join(format!(".creds-{number}-{email}.enc"));
    let from_file = match fs::read_to_string(&enc) {
        Ok(text) => decode_base64(text.trim()).and_then(|b| String::from_utf8(b).ok()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("could not read claude-swap's backup ({e})")),
    };
    let text = match from_file.filter(|t| !t.is_empty()) {
        Some(text) => text,
        None => match keychain(&format!("account-{number}-{email}")) {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(None),
            Err(e) => return Err(format!("could not read claude-swap's backup ({e})")),
        },
    };
    let serde_json::Value::Object(mut item) = serde_json::from_str(&text)
        .map_err(|_| "claude-swap's backup is not a JSON object".to_owned())?
    else {
        return Err("claude-swap's backup is not a JSON object".to_owned());
    };
    match item.remove(secret::LOGIN_KEY) {
        Some(login) if login.is_object() => Ok(Some(login)),
        Some(_) => Err("claude-swap's backup holds no valid login".to_owned()),
        None => Ok(None),
    }
}

/// Standard base64 with padding, as Python's `b64encode` writes it; `None`
/// for anything else.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (i, chunk) in text.chunks(4).enumerate() {
        let last = i == text.len() / 4 - 1;
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = n << 6 | u32::from(value(c)?);
        }
        n <<= 6 * pad;
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    Some(out)
}

/// The account id in claude-swap's copy of an account's `.claude.json`.
fn swap_account_id(from: &Path, number: u32, email: &str) -> Option<String> {
    account::claude_id(&swap_config(from, number, email))
        .ok()
        .flatten()
}

fn swap_config(from: &Path, number: u32, email: &str) -> PathBuf {
    from.join("configs")
        .join(format!(".claude-config-{number}-{email}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYCHAIN_LOGIN: &str = r#"{"claudeAiOauth":{"refreshToken":"fake-keychain"}}"#;

    /// claude-swap's Keychain on macOS, faked: one item per backup user.
    fn keychain(user: &str) -> io::Result<Option<String>> {
        Ok((user == "account-1-one@example.com").then(|| KEYCHAIN_LOGIN.to_owned()))
    }

    fn refresh_token(login: Option<serde_json::Value>) -> String {
        login.unwrap()["refreshToken"].as_str().unwrap().to_owned()
    }

    #[test]
    fn reads_the_keychain_when_there_is_no_backup_file() {
        let from = tempfile::tempdir().unwrap();
        let login = swap_login(from.path(), 1, "one@example.com", &keychain).unwrap();
        assert_eq!(refresh_token(login), "fake-keychain");
        assert_eq!(
            swap_login(from.path(), 2, "two@example.com", &keychain).unwrap(),
            None
        );
    }

    #[test]
    fn the_backup_file_wins_over_the_keychain_unless_it_is_not_base64() {
        let from = tempfile::tempdir().unwrap();
        let dir = from.path().join("credentials");
        fs::create_dir_all(&dir).unwrap();
        let enc = dir.join(".creds-1-one@example.com.enc");
        // {"claudeAiOauth":{"refreshToken":"fake-file"}}, as b64encode writes it.
        fs::write(
            &enc,
            "eyJjbGF1ZGVBaU9hdXRoIjp7InJlZnJlc2hUb2tlbiI6ImZha2UtZmlsZSJ9fQ==\n",
        )
        .unwrap();
        let login = swap_login(from.path(), 1, "one@example.com", &keychain).unwrap();
        assert_eq!(refresh_token(login), "fake-file");

        for corrupt in ["!!!!", ""] {
            fs::write(&enc, corrupt).unwrap();
            let login = swap_login(from.path(), 1, "one@example.com", &keychain).unwrap();
            assert_eq!(refresh_token(login), "fake-keychain", "{corrupt:?}");
        }
    }

    #[test]
    fn a_keychain_that_does_not_answer_says_so_without_the_secret() {
        let from = tempfile::tempdir().unwrap();
        let locked = |_: &str| -> io::Result<Option<String>> {
            Err(io::Error::other("security exited Some(36)"))
        };
        let why = swap_login(from.path(), 1, "one@example.com", &locked).unwrap_err();
        assert!(why.contains("could not read"), "{why}");
    }

    #[test]
    fn an_api_key_is_no_login() {
        let from = tempfile::tempdir().unwrap();
        let api_key = |_: &str| Ok(Some("sk-ant-fake".to_owned()));
        assert!(swap_login(from.path(), 1, "one@example.com", &api_key).is_err());
        let other = |_: &str| Ok(Some(r#"{"mcpOAuth":{}}"#.to_owned()));
        assert_eq!(
            swap_login(from.path(), 1, "one@example.com", &other).unwrap(),
            None
        );
    }

    #[test]
    fn decodes_base64_as_python_writes_it() {
        for (text, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(decode_base64(text).unwrap(), plain.as_bytes(), "{text}");
        }
        for bad in ["Zg=", "Zg==Zm9v", "Z===", "Zm9!"] {
            assert_eq!(decode_base64(bad), None, "{bad}");
        }
    }
}
