//! Applies the switching policy to the default account. Runs inside commands
//! that already run (`hats run`, the status line wrapper), never on its own.

use std::time::SystemTime;

use crate::account::{Account, Harness};
use crate::config::{HarnessConfig, Mode};
use crate::limits::{self, Reading};
use crate::policy::{self, Candidate, Reason};
use crate::registry::{self, Switch};
use crate::store::Store;

/// One account with what is known about it.
pub struct Status {
    pub account: Account,
    pub id: Option<String>,
    pub reading: Option<Reading>,
}

impl Status {
    pub fn used(&self, now: SystemTime) -> Option<f64> {
        self.reading.as_ref().map(|r| r.binding_used(now))
    }

    fn candidate(&self, now: SystemTime) -> Candidate {
        Candidate {
            label: self.account.label.clone(),
            logged_in: self.id.is_some(),
            used: self.used(now),
        }
    }
}

/// Every account of `harness` with its id and last limits, in rotation order.
/// The harness's own directory is listed only if it exists.
pub fn statuses(
    store: &Store,
    harness: Harness,
    config: &HarnessConfig,
) -> Result<Vec<Status>, String> {
    let claude = match harness {
        Harness::Claude => limits::claude_readings(store),
        Harness::Codex => Default::default(),
    };
    let mut all = Vec::new();
    for account in Account::all(harness, store).map_err(|e| e.to_string())? {
        if account.is_default && !account.dir.exists() {
            continue;
        }
        let id = account.provider_id(store).unwrap_or(None);
        let reading = match (harness, &id) {
            (Harness::Claude, Some(id)) => claude.get(id).cloned(),
            (Harness::Codex, Some(_)) => limits::codex_reading(&account.dir.join("sessions"))
                .ok()
                .flatten(),
            _ => None,
        };
        all.push(Status {
            account,
            id,
            reading,
        });
    }
    let order = policy::rotation(all.iter().map(|s| s.account.label.as_str()), &config.order)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    all.sort_by_key(|s| order.iter().position(|l| *l == s.account.label));
    Ok(all)
}

/// The account to move to from `from`, with the reason, if `from` passed the
/// threshold (or `forced` says why it must move) and another account has room.
pub fn next_account<'a>(
    statuses: &'a [Status],
    from: &str,
    config: &HarnessConfig,
    now: SystemTime,
    forced: Option<Reason>,
) -> Option<(&'a Status, Reason)> {
    let current = statuses.iter().find(|s| s.account.label == from)?;
    let reason = match forced {
        Some(reason) => reason,
        None => policy::trigger(current.used(now)?, config.threshold)?,
    };
    let candidates: Vec<Candidate> = statuses.iter().map(|s| s.candidate(now)).collect();
    let to = policy::pick(from, &candidates, config)?;
    statuses
        .iter()
        .find(|s| s.account.label == to)
        .map(|s| (s, reason))
}

/// In auto mode, moves the default account off one that passed the
/// threshold, or off one `forced` says must go. Returns the new default when
/// it changed.
pub fn check_default(
    store: &Store,
    harness: Harness,
    config: &HarnessConfig,
    now: SystemTime,
    forced: Option<Reason>,
) -> Result<Option<(Account, Reason)>, String> {
    if config.mode != Mode::Auto {
        return Ok(None);
    }
    let current = store.current(harness)?;
    let statuses = statuses(store, harness, config)?;
    let Some((next, reason)) = next_account(&statuses, &current.label, config, now, forced) else {
        return Ok(None);
    };
    let id = next.id.as_deref().expect("candidates are logged in");
    make_default(store, &next.account, id, reason, now)?;
    Ok(Some((next.account.clone(), reason)))
}

/// Makes `account` the default for new sessions and records why.
pub fn make_default(
    store: &Store,
    account: &Account,
    id: &str,
    reason: Reason,
    now: SystemTime,
) -> Result<(), String> {
    store.set_current(account).map_err(|e| e.to_string())?;
    let line = Switch::new(
        now,
        account.harness,
        id,
        &account.label,
        &account.dir,
        None,
        Some(reason),
    );
    registry::append(&store.registry(), &line).map_err(|e| e.to_string())
}
