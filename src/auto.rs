//! Applies the switching policy to the default account. Runs inside commands
//! that already run (`hats run`, the status line wrapper), never on its own.

use std::time::SystemTime;

use crate::account::{Account, Harness, Identity};
use crate::config::{HarnessConfig, Mode};
use crate::limits::{self, Reading};
use crate::policy::{self, Candidate, Reason};
use crate::registry::{self, Switch};
use crate::seat;
use crate::store::Store;

/// One account with what is known about it.
pub struct Status {
    pub account: Account,
    pub id: Option<String>,
    pub identity: Identity,
    pub reading: Option<Reading>,
}

impl Status {
    /// Highest use now, or `None` when nothing is known: a learned limit
    /// that expired leaves no numbers behind.
    pub fn used(&self, now: SystemTime) -> Option<f64> {
        self.reading
            .as_ref()
            .filter(|r| r.limited(now) || !r.windows.is_empty())
            .map(|r| r.binding_used(now))
    }

    /// Whether the provider refused its login: it has to sign in again.
    pub fn login_refused(&self) -> bool {
        self.reading
            .as_ref()
            .is_some_and(|r| r.login_refused_at.is_some())
    }

    fn candidate(&self, now: SystemTime) -> Candidate {
        Candidate {
            label: self.account.label.clone(),
            // A refused login would leave every session without one.
            logged_in: self.id.is_some() && !self.login_refused(),
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
        let (id, mut identity) = match account.login(store).unwrap_or(None) {
            Some(login) => (Some(login.id), login.identity),
            None => (None, Identity::default()),
        };
        // Claude Code's own directory is the seat: it shows as the account
        // whose login sits in it, and as a row of its own only while no
        // account holds that login.
        if harness == Harness::Claude
            && account.is_default
            && id
                .as_ref()
                .is_none_or(|id| seat::holder(store, id).is_some())
        {
            continue;
        }
        let reading = match (harness, &id) {
            (Harness::Claude, Some(id)) => claude.get(id).cloned(),
            (Harness::Codex, Some(_)) => limits::codex_reading(&account.dir.join("sessions"))
                .ok()
                .flatten(),
            _ => None,
        };
        if let Some(plan) = reading.as_ref().and_then(|r| r.plan.clone()) {
            identity.plan.get_or_insert(plan);
        }
        all.push(Status {
            account,
            id,
            identity,
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

/// Whether auto switching has an account to move to from `from`: one signed
/// in with a login the provider takes and not turned off, whatever its room.
pub fn has_destination(
    statuses: &[Status],
    from: &str,
    config: &HarnessConfig,
    now: SystemTime,
) -> bool {
    let candidates: Vec<Candidate> = statuses.iter().map(|s| s.candidate(now)).collect();
    policy::has_destination(from, &candidates, config)
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
        // Every session in the seat fails until another login sits there.
        None if current.login_refused() => Reason::Login,
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
/// threshold, or off one `forced` says must go; with `only_from`, only while
/// that account is still the default. Returns the new default when it
/// changed.
pub fn check_default(
    store: &Store,
    harness: Harness,
    config: &HarnessConfig,
    now: SystemTime,
    forced: Option<Reason>,
    only_from: Option<&str>,
) -> Result<Option<(Account, Reason)>, String> {
    if config.mode != Mode::Auto {
        return Ok(None);
    }
    let _lock = store.lock_current().map_err(|e| e.to_string())?;
    let current = store.current(harness)?;
    if only_from.is_some_and(|from| from != current.label) {
        return Ok(None);
    }
    let statuses = statuses(store, harness, config)?;
    let Some((next, reason)) = next_account(&statuses, &current.label, config, now, forced) else {
        return Ok(None);
    };
    let id = next.id.as_deref().expect("candidates are logged in");
    make_default(store, &next.account, id, reason, now)?;
    Ok(Some((next.account.clone(), reason)))
}

/// In auto mode, moves the default account off `label` right after the user
/// disabled it. Only then: an account chosen by hand later stays.
pub fn leave_disabled(
    store: &Store,
    harness: Harness,
    config: &HarnessConfig,
    label: &str,
) -> Result<Option<(Account, Reason)>, String> {
    check_default(
        store,
        harness,
        config,
        SystemTime::now(),
        Some(Reason::Disabled),
        Some(label),
    )
}

/// The user's choice of default account, recorded as `manual`. Choosing the
/// account already in use changes and records nothing.
pub fn choose(store: &Store, account: &Account, id: &str, now: SystemTime) -> Result<(), String> {
    let _lock = store.lock_current().map_err(|e| e.to_string())?;
    if store.current(account.harness)?.label == account.label {
        return Ok(());
    }
    make_default(store, account, id, Reason::Manual, now)
}

/// Makes `account` the one sessions use and records why: for Claude Code its
/// login takes the seat, so running sessions move too; for Codex new
/// sessions start with it. The caller holds `lock_current`.
fn make_default(
    store: &Store,
    account: &Account,
    id: &str,
    reason: Reason,
    now: SystemTime,
) -> Result<(), String> {
    match account.harness {
        Harness::Claude => {
            seat::sit(store, account)?;
        }
        Harness::Codex => store.set_current(account).map_err(|e| e.to_string())?,
    }
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
