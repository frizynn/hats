//! Claude Code hooks. They inherit the session's environment, so
//! `CLAUDE_CONFIG_DIR` names the account, and get the session on stdin.
//!
//! - `SessionStart`: records which account the session runs with, and starts
//!   a `hats run` parent's clock for closing. A resume under another account
//!   writes a new line; the newest line wins.
//! - `UserPromptSubmit`, `Stop`, `StopFailure`: tell a `hats run` parent when
//!   the agent is working or idle; a rate limit error marks the account full
//!   and moves the seat off it.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::account::{Account, Harness};
use crate::config::HarnessConfig;
use crate::limits;
use crate::policy::Reason;
use crate::registry::{self, Switch};
use crate::store::Store;
use crate::{auto, seat, supervisor};

/// Hook payloads are a few kilobytes; anything larger is not ours.
const MAX_INPUT: u64 = 1 << 20;

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

pub fn prompt(store: &Store) -> Result<(), String> {
    supervisor::mark_busy(store).map_err(|e| e.to_string())
}

pub fn stop(
    store: &Store,
    config: &HarnessConfig,
    config_dir: Option<OsString>,
    stdin: impl Read,
    now: SystemTime,
) -> Result<(), String> {
    let input = read_input(stdin)?;
    let rate_limited = input.hook_event_name.as_deref() == Some("StopFailure")
        && input.error.as_deref() == Some("rate_limit");
    // Claude Code keeps a login it read for about 30 seconds: an error right
    // after the seat changed hands may be the account that left.
    let just_seated = config_dir.as_ref().is_none_or(|d| d.is_empty())
        && seat::changed_at(store)
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|since| since < seat::SETTLING);
    if rate_limited && !just_seated {
        let account = session_account(store, config_dir);
        // Known as full from now on, whether or not it is the default.
        if let Some(id) = account.provider_id(store).map_err(|e| e.to_string())? {
            limits::mark_limited(store, &account, &id, now).map_err(|e| e.to_string())?;
        }
        auto::check_default(
            store,
            Harness::Claude,
            config,
            now,
            Some(Reason::Limit),
            Some(&account.label),
        )?;
    }
    let turn = supervisor::Turn::new(input.session_id, input.transcript_path, now);
    supervisor::mark_idle(store, &turn).map_err(|e| e.to_string())
}
