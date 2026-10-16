//! Accounts are configuration directories. Each one is a full `CLAUDE_CONFIG_DIR`
//! or `CODEX_HOME` where the harness itself logs in and keeps its own
//! credentials; this tool only creates the directory, shares the user's
//! settings into it and picks which one a harness starts with.

use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::Store;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
}

/// Entries linked from the harness's default directory into every account so
/// settings, instructions and history stay one set. Everything else (the
/// login, caches, per-install state) stays per account.
const CLAUDE_SHARED_FILES: &[&str] = &[
    "settings.json",
    "CLAUDE.md",
    "keybindings.json",
    "history.jsonl",
];
const CLAUDE_SHARED_DIRS: &[&str] = &[
    "agents",
    "commands",
    "skills",
    "plugins",
    "output-styles",
    "projects",
    "file-history",
    "todos",
    "plans",
];
// Codex sessions stay per account: each rollout header names the account that
// created it, which is how the account id is read without touching auth.json.
const CODEX_SHARED_FILES: &[&str] = &["config.toml", "AGENTS.md", "hooks.json", "history.jsonl"];
const CODEX_SHARED_DIRS: &[&str] = &["rules", "skills", "prompts", "plugins"];

impl Harness {
    pub const ALL: [Harness; 2] = [Harness::Claude, Harness::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    /// The program this tool execs, looked up on `PATH`.
    pub fn program(self) -> &'static str {
        self.name()
    }

    pub fn env_var(self) -> &'static str {
        match self {
            Harness::Claude => "CLAUDE_CONFIG_DIR",
            Harness::Codex => "CODEX_HOME",
        }
    }

    pub fn login_args(self) -> &'static [&'static str] {
        match self {
            Harness::Claude => &["auth", "login"],
            Harness::Codex => &["login"],
        }
    }

    pub fn logout_args(self) -> &'static [&'static str] {
        match self {
            Harness::Claude => &["auth", "logout"],
            Harness::Codex => &["logout"],
        }
    }

    fn default_dir_name(self) -> &'static str {
        match self {
            Harness::Claude => ".claude",
            Harness::Codex => ".codex",
        }
    }

    /// The shared transcript directory, created up front so the first session
    /// of a new account already writes into the common history.
    fn history_dir(self) -> Option<&'static str> {
        match self {
            Harness::Claude => Some("projects"),
            Harness::Codex => None,
        }
    }

    fn shared(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            Harness::Claude => (CLAUDE_SHARED_FILES, CLAUDE_SHARED_DIRS),
            Harness::Codex => (CODEX_SHARED_FILES, CODEX_SHARED_DIRS),
        }
    }
}

/// The label that names the harness's own directory (`~/.claude`, `~/.codex`),
/// so an existing login keeps working as one of the accounts.
pub const DEFAULT_LABEL: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub harness: Harness,
    pub label: String,
    pub dir: PathBuf,
    /// The harness's default directory: started with its variable unset.
    pub is_default: bool,
}

impl Account {
    pub fn default_for(harness: Harness, store: &Store) -> Self {
        Self {
            harness,
            label: DEFAULT_LABEL.to_owned(),
            dir: store.home.join(harness.default_dir_name()),
            is_default: true,
        }
    }

    pub fn managed(harness: Harness, label: &str, store: &Store) -> Result<Self, String> {
        validate_label(label)?;
        Ok(Self {
            harness,
            label: label.to_owned(),
            dir: store.accounts_dir(harness).join(label),
            is_default: false,
        })
    }

    /// Resolves a label to an existing account.
    pub fn find(harness: Harness, label: &str, store: &Store) -> Result<Self, String> {
        if label == DEFAULT_LABEL {
            return Ok(Self::default_for(harness, store));
        }
        let account = Self::managed(harness, label, store)?;
        if !account.dir.is_dir() {
            return Err(format!(
                "no {} account named '{label}' (see `agent-accounts list`)",
                harness.name()
            ));
        }
        Ok(account)
    }

    /// Maps the directory a harness is running with back to its account.
    pub fn from_config_dir(harness: Harness, dir: Option<&Path>, store: &Store) -> Self {
        let Some(dir) = dir else {
            return Self::default_for(harness, store);
        };
        let label = match dir.parent() {
            Some(parent) if parent == store.accounts_dir(harness) => dir.file_name(),
            _ => None,
        }
        .map(|name| name.to_string_lossy().into_owned());
        Self {
            harness,
            label: label.unwrap_or_else(|| dir.to_string_lossy().into_owned()),
            dir: dir.to_path_buf(),
            is_default: false,
        }
    }

    /// Managed accounts on disk, sorted by label, after the default one.
    pub fn all(harness: Harness, store: &Store) -> io::Result<Vec<Self>> {
        let mut accounts = vec![Self::default_for(harness, store)];
        let entries = match fs::read_dir(store.accounts_dir(harness)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(accounts),
            Err(e) => return Err(e),
        };
        let mut managed = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() && validate_label(&name).is_ok() {
                managed.push(Self::managed(harness, &name, store).expect("validated label"));
            }
        }
        managed.sort_by(|a, b| a.label.cmp(&b.label));
        accounts.extend(managed);
        Ok(accounts)
    }

    /// Creates the directory and links the shared entries that exist in the
    /// harness's default directory. Idempotent: existing entries are kept.
    pub fn create(&self, store: &Store) -> io::Result<()> {
        if self.is_default {
            return Ok(());
        }
        create_private_dir(&self.dir)?;
        let source = Self::default_for(self.harness, store).dir;
        if let Some(history) = self.harness.history_dir() {
            fs::create_dir_all(source.join(history))?;
        }
        let (files, dirs) = self.harness.shared();
        for name in files.iter().chain(dirs) {
            let target = source.join(name);
            let link = self.dir.join(name);
            if target.exists() && fs::symlink_metadata(&link).is_err() {
                std::os::unix::fs::symlink(&target, &link)?;
            }
        }
        Ok(())
    }

    /// Stable id of the logged-in provider account, or `None` before login.
    /// Reads only identity fields, never credentials.
    pub fn provider_id(&self, store: &Store) -> io::Result<Option<String>> {
        let key = match self.harness {
            Harness::Claude => claude_identity(&self.claude_global_config(store))?,
            Harness::Codex => codex_identity(&self.dir.join("sessions"))?,
        };
        Ok(key.map(|key| short_hash(&key)))
    }

    /// Claude Code keeps its global config at `$CLAUDE_CONFIG_DIR/.claude.json`,
    /// or at `~/.claude.json` when the variable is unset.
    fn claude_global_config(&self, store: &Store) -> PathBuf {
        if self.is_default {
            store.home.join(".claude.json")
        } else {
            self.dir.join(".claude.json")
        }
    }
}

pub fn validate_label(label: &str) -> Result<(), String> {
    let ok = !label.is_empty()
        && label.len() <= 32
        && label != DEFAULT_LABEL
        && label.starts_with(|c: char| c.is_ascii_alphanumeric())
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid label '{label}': use 1-32 letters, digits, '-', '_' or '.', \
             starting with a letter or digit ('{DEFAULT_LABEL}' is reserved)"
        ))
    }
}

pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// First 16 hex digits of SHA-256: stable, short, and not reversible to the
/// provider's identifiers.
pub fn short_hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Deserialize)]
struct ClaudeGlobalConfig {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<ClaudeOauthAccount>,
}

#[derive(Deserialize)]
struct ClaudeOauthAccount {
    #[serde(rename = "accountUuid")]
    account_uuid: String,
    #[serde(rename = "organizationUuid", default)]
    organization_uuid: Option<String>,
}

/// `"<accountUuid>:<organizationUuid>"`: one login can belong to several
/// organizations, and plan limits are per organization.
fn claude_identity(global_config: &Path) -> io::Result<Option<String>> {
    let bytes = match fs::read(global_config) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let config: ClaudeGlobalConfig = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(config.oauth_account.map(|a| {
        format!(
            "{}:{}",
            a.account_uuid,
            a.organization_uuid.unwrap_or_default()
        )
    }))
}

#[derive(Deserialize)]
struct CodexRolloutHeader {
    payload: CodexSessionMeta,
}

#[derive(Deserialize)]
struct CodexSessionMeta {
    creator_account_id: Option<String>,
}

/// `creator_account_id` from the header of the newest rollout. Rollouts live
/// in `sessions/YYYY/MM/DD/rollout-<timestamp>-<id>.jsonl`, so the newest is
/// the greatest name at each level.
fn codex_identity(sessions: &Path) -> io::Result<Option<String>> {
    let mut path = sessions.to_path_buf();
    for _ in 0..4 {
        match greatest_entry(&path)? {
            Some(next) => path = next,
            None => return Ok(None),
        }
    }
    let mut first = String::new();
    BufReader::new(fs::File::open(&path)?).read_line(&mut first)?;
    let header: CodexRolloutHeader = match serde_json::from_str(&first) {
        Ok(header) => header,
        Err(_) => return Ok(None),
    };
    Ok(header.payload.creator_account_id)
}

fn greatest_entry(dir: &Path) -> io::Result<Option<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut best: Option<std::ffi::OsString> = None;
    for entry in entries {
        let name = entry?.file_name();
        if !name.to_string_lossy().starts_with('.') && best.as_ref().is_none_or(|b| name > *b) {
            best = Some(name);
        }
    }
    Ok(best.map(|name| dir.join(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        for ok in ["work", "a", "Team-2", "x.y_z"] {
            assert!(validate_label(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "default",
            "-x",
            ".hidden",
            "a/b",
            "sp ace",
            &"x".repeat(33),
        ] {
            assert!(validate_label(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn short_hash_is_sha256_prefix() {
        // sha256("abc") = ba7816bf8f01cfea...
        assert_eq!(short_hash("abc"), "ba7816bf8f01cfea");
    }
}
