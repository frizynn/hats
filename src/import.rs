//! `hats import claude-swap`: takes claude-swap's account order, aliases,
//! switching settings and last limits. It reads `sequence.json`,
//! `settings.json`, the identity in claude-swap's copies of each account's
//! `.claude.json` and the percentages in `cache/usage.json`; its stored
//! tokens stay where they are, and each account signs in again through the
//! harness's own login, except the one already in `~/.claude`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::account::{self, Account, DEFAULT_LABEL, Harness, validate_label};
use crate::config;
use crate::limits::{self, LimitWindow};
use crate::registry;
use crate::store::Store;

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

pub fn claude_swap(store: &Store, from: &Path) -> Result<(), String> {
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
        if account
            .provider_id(store)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            to_sign_in.push((label.clone(), swap.email.clone()));
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
    if !to_sign_in.is_empty() {
        println!(
            "\nSign in to each account with Claude Code's own login (rename first with `hats rm` and `hats add` if you want another label):"
        );
        for (label, email) in &to_sign_in {
            println!("  hats login {label}    # {email}");
        }
    }
    println!(
        "\nWhen they are all signed in, stop `cswap auto` and run `cswap purge` to delete claude-swap's stored tokens."
    );
    Ok(())
}

/// The account id in claude-swap's copy of an account's `.claude.json`. Its
/// credentials folder is never read.
fn swap_account_id(from: &Path, number: u32, email: &str) -> Option<String> {
    let copy = from
        .join("configs")
        .join(format!(".claude-config-{number}-{email}.json"));
    account::claude_id(&copy).ok().flatten()
}
