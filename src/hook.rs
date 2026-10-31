//! Claude Code hooks. They inherit the session's environment, so
//! `CLAUDE_CONFIG_DIR` names the account, and get the session on stdin.
//!
//! - `SessionStart`: records which account the session runs with, and starts
//!   a `hats run` parent's clock for closing. A resume under another account
//!   writes a new line; the newest line wins.
//! - `UserPromptSubmit`, `Stop`, `StopFailure`: tell a `hats run` parent when
//!   the agent is working or idle. A turn cut by a rate limit or by a login
//!   the provider refused (`error` `rate_limit` or `authentication_failed`,
//!   measured on 2.1.316 with a 401) marks the account and moves the seat off
//!   it; with no account to move to, Claude Code shows a notification.
//! - `StopFailure` again, run in the background with `asyncRewake`: `revive`
//!   waits until the cut session has a working account and wakes it by
//!   exiting 2 (measured on 2.1.316: the session runs a new turn with the new
//!   login once its `.credentials.json` changed). One waiter per session; a
//!   prompt typed meanwhile cancels it, and it leaves with Claude Code. Esc
//!   runs no hook, so a turn the user cut is never woken.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::account::{Account, Harness};
use crate::config::{Config, HarnessConfig};
use crate::limits;
use crate::policy::Reason;
use crate::registry::{self, Switch};
use crate::store::Store;
use crate::{auto, poll, process, seat, supervisor};

/// Hook payloads are a few kilobytes; anything larger is not ours.
const MAX_INPUT: u64 = 1 << 20;
/// How often a waiting `revive` looks again.
const REVIVE_EVERY: Duration = Duration::from_secs(1);

#[derive(Deserialize)]
struct HookInput {
    session_id: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    transcript_path: Option<PathBuf>,
}

fn read_input(stdin: impl Read) -> Result<HookInput, String> {
    let mut input = Vec::new();
    stdin
        .take(MAX_INPUT)
        .read_to_end(&mut input)
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&input).map_err(|e| format!("hook input: {e}"))
}

fn session_account(store: &Store, config_dir: Option<OsString>) -> Account {
    let config_dir = config_dir.filter(|dir| !dir.is_empty());
    Account::from_config_dir(Harness::Claude, config_dir.as_deref().map(Path::new), store)
}

pub fn session_start(
    store: &Store,
    config_dir: Option<OsString>,
    stdin: impl Read,
    now: SystemTime,
) -> Result<(), String> {
    let input = read_input(stdin)?;
    // Compaction keeps the session and its account, and may run mid-turn.
    if input.source.as_deref() == Some("compact") {
        return Ok(());
    }
    let recorded = record_account(store, config_dir, &input.session_id, now);
    let turn = supervisor::Turn::new(input.session_id, input.transcript_path, now);
    supervisor::mark_opened(store, &turn).map_err(|e| e.to_string())?;
    recorded
}

fn record_account(
    store: &Store,
    config_dir: Option<OsString>,
    session: &str,
    now: SystemTime,
) -> Result<(), String> {
    let account = session_account(store, config_dir);
    let Some(id) = account.provider_id(store).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    store.ensure_state().map_err(|e| e.to_string())?;
    let line = Switch::new(
        now,
        Harness::Claude,
        &id,
        &account.label,
        &account.dir,
        Some(session),
        None,
    );
    registry::append(&store.registry(), &line).map_err(|e| e.to_string())
}

/// The agent works again; a session the user took over is not woken.
pub fn prompt(store: &Store, stdin: impl Read) -> Result<(), String> {
    supervisor::mark_busy(store).map_err(|e| e.to_string())?;
    if let Ok(input) = read_input(stdin) {
        let _ = fs::remove_file(waiter_path(store, &input.session_id));
    }
    Ok(())
}

/// What cut a turn: an error moving to another account can fix.
fn cut_by(input: &HookInput) -> Option<Reason> {
    if input.hook_event_name.as_deref() != Some("StopFailure") {
        return None;
    }
    match input.error.as_deref()? {
        "rate_limit" => Some(Reason::Limit),
        "authentication_failed" => Some(Reason::Login),
        _ => None,
    }
}

/// Marks the account a cut turn ran on and moves the seat off it. Returns
/// whether it is stuck there: no other account to move to. Two sessions cut
/// together change account once (see `auto::check_default`).
fn handle_cut(
    store: &Store,
    config: &HarnessConfig,
    config_dir: Option<OsString>,
    reason: Reason,
    now: SystemTime,
) -> Result<bool, String> {
    // Claude Code keeps a login it read for about 30 seconds: an error right
    // after the seat changed hands may be the account that left.
    let just_seated = config_dir.as_ref().is_none_or(|d| d.is_empty())
        && seat::changed_at(store)
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|since| since < seat::SETTLING);
    if just_seated {
        return Ok(false);
    }
    let account = session_account(store, config_dir);
    // Known as unusable from now on, whether or not it is the default.
    if let Some(id) = account.provider_id(store).map_err(|e| e.to_string())? {
        let marked = match reason {
            Reason::Login => limits::mark_refused(store, &account, &id, now),
            _ => limits::mark_limited(store, &account, &id, now),
        };
        marked.map_err(|e| e.to_string())?;
    }
    auto::check_default(
        store,
        Harness::Claude,
        config,
        now,
        Some(reason),
        Some(&account.label),
    )?;
    let current = store.current(Harness::Claude)?;
    let statuses = auto::statuses(store, Harness::Claude, config)?;
    Ok(current.label == account.label
        && !auto::has_destination(&statuses, &current.label, config, now))
}

pub fn stop(
    store: &Store,
    config: &HarnessConfig,
    config_dir: Option<OsString>,
    stdin: impl Read,
    now: SystemTime,
) -> Result<(), String> {
    let input = read_input(stdin)?;
    if let Some(reason) = cut_by(&input)
        && handle_cut(store, config, config_dir, reason, now)?
    {
        // Claude Code writes it to the terminal: a desktop notification.
        let text =
            "hats: no other account can take over; this session goes on by itself when one is free";
        let sequence = format!("\u{1b}]9;{text}\u{7}\u{1b}]777;notify;hats;{text}\u{7}");
        println!("{}", serde_json::json!({ "terminalSequence": sequence }));
    }
    let turn = supervisor::Turn::new(input.session_id, input.transcript_path, now);
    supervisor::mark_idle(store, &turn).map_err(|e| e.to_string())
}

fn waiter_path(store: &Store, session: &str) -> PathBuf {
    // Session ids are UUIDs; anything else must not name a path.
    let name: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    store.state.join("revive").join(name)
}

/// The waiter of `session`: this process, unless one still running already
/// waits for it.
fn claim(path: &Path) -> Result<bool, String> {
    let me = std::process::id();
    if let Some(pid) = fs::read_to_string(path)
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        && pid != me
        && process::alive(pid)
    {
        return Ok(false);
    }
    fs::create_dir_all(path.parent().expect("in revive/")).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("{me}.tmp"));
    fs::write(&tmp, me.to_string()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    // Two waiters started together: the last rename wins.
    Ok(fs::read_to_string(path).is_ok_and(|t| t.trim() == me.to_string()))
}

/// `StopFailure` with `asyncRewake`: once the session cut by a rate limit or
/// a refused login has a working account, the words that wake it (Claude Code
/// shows them to the agent); `None` when it is not to be woken.
pub fn revive(
    store: &Store,
    config_dir: Option<OsString>,
    stdin: impl Read,
) -> Result<Option<String>, String> {
    let input = read_input(stdin)?;
    let Some(reason) = cut_by(&input) else {
        return Ok(None);
    };
    let config = Config::load(&store.config)?;
    handle_cut(
        store,
        &config.claude,
        config_dir.clone(),
        reason,
        SystemTime::now(),
    )?;
    let path = waiter_path(store, &input.session_id);
    if !claim(&path)? {
        return Ok(None);
    }
    // SAFETY: getppid(2) cannot fail.
    let claude = unsafe { libc::getppid() };
    let in_seat = config_dir.as_ref().is_none_or(|d| d.is_empty());
    loop {
        std::thread::sleep(REVIVE_EVERY);
        let mine =
            fs::read_to_string(&path).is_ok_and(|t| t.trim() == std::process::id().to_string());
        // SAFETY: as above.
        if !mine || unsafe { libc::getppid() } != claude {
            // A prompt took over, or Claude Code is gone.
            return Ok(None);
        }
        let now = SystemTime::now();
        let config = Config::load(&store.config)?;
        // Nothing else may run while every session is stopped: this asks for
        // the limits, and moves the seat once an account has room.
        if poll::due(store, now) {
            poll::poll(store, &config.claude, now);
        } else {
            let _ = auto::check_default(store, Harness::Claude, &config.claude, now, None, None);
        }
        let account = if in_seat {
            seat::current(store)
        } else {
            session_account(store, config_dir.clone())
        };
        let usable = auto::statuses(store, Harness::Claude, &config.claude)?
            .iter()
            .find(|s| s.account.label == account.label)
            .is_some_and(|s| {
                s.id.is_some() && !s.login_refused() && s.used(now).is_none_or(|u| u < 100.0)
            });
        let settled = seat::changed_at(store)
            .and_then(|at| now.duration_since(at).ok())
            .is_none_or(|since| since >= seat::SETTLING);
        if usable && settled {
            let _ = fs::remove_file(&path);
            let why = match reason {
                Reason::Login => "the provider refused the login of the account it ran on",
                _ => "the account it ran on hit its limit",
            };
            return Ok(Some(format!(
                "hats: this session stopped because {why}; it now runs on '{}'. Continue the task where it stopped.",
                account.label
            )));
        }
    }
}
