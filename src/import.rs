//! `hats import claude-swap`: takes claude-swap's account order, aliases and
//! switching settings. It reads only `sequence.json` and `settings.json`;
//! claude-swap's stored tokens stay where they are, and each account signs in
//! again through the harness's own login.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::account::{Account, Harness, validate_label};
use crate::config;
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

    // claude-swap's rotation order, then any account it lists but not in order.
    let mut numbers: Vec<u32> = sequence.sequence.clone();
    for key in sequence.accounts.keys() {
        if let Ok(n) = key.parse::<u32>()
            && !numbers.contains(&n)
        {
            numbers.push(n);
        }
    }

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
        let account = Account::managed(Harness::Claude, &label, store)?;
        let created = !account.dir.exists();
        account.create(store).map_err(|e| e.to_string())?;
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
