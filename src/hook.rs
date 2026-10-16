//! Claude Code `SessionStart` hook: records which account a session runs with.
//!
//! Claude Code passes the session on stdin and the hook inherits the session's
//! environment, so `CLAUDE_CONFIG_DIR` names the account. A resume under
//! another account writes a new line, and the newest line for a session wins.

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use serde::Deserialize;

use crate::account::{Account, Harness};
use crate::registry::{self, Switch};
use crate::store::Store;

/// Hook payloads are a few hundred bytes; anything larger is not ours.
const MAX_INPUT: u64 = 1 << 20;

#[derive(Deserialize)]
struct SessionStartInput {
    session_id: String,
    #[serde(default)]
    source: Option<String>,
}

pub fn session_start(
    store: &Store,
    config_dir: Option<OsString>,
    stdin: impl Read,
    now: SystemTime,
) -> Result<(), String> {
    let mut input = Vec::new();
    stdin
        .take(MAX_INPUT)
        .read_to_end(&mut input)
        .map_err(|e| e.to_string())?;
    let input: SessionStartInput =
        serde_json::from_slice(&input).map_err(|e| format!("hook input: {e}"))?;
    // Compaction keeps the session and its account.
    if input.source.as_deref() == Some("compact") {
        return Ok(());
    }
    let config_dir = config_dir.filter(|dir| !dir.is_empty());
    let account =
        Account::from_config_dir(Harness::Claude, config_dir.as_deref().map(Path::new), store);
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
        Some(&input.session_id),
    );
    registry::append(&store.registry(), &line).map_err(|e| e.to_string())
}
