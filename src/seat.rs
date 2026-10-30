//! The seat: Claude Code's own directory (`~/.claude`), where every session
//! runs. Changing account puts another login in the seat; running sessions
//! read the stored login again (Claude Code drops its cached token after
//! about 30 s, or at once when `.credentials.json` changes) and go on with
//! the new account, with no restart, like claude-swap did.
//!
//! Each account directory keeps its own login, the one `hats login` made or
//! the one the seat had when it last left it, so the login that leaves the
//! seat is parked first and nothing is lost. A login found in the seat that
//! no account holds (signed in with `/login` in a plain session) gets an
//! account of its own before it leaves.
//!
//! The change runs under Claude Code's own refresh locks (measured on 2.1.304:
//! `<dir>/.oauth_refresh.lock` then `<dir>.lock`, both `proper-lockfile`
//! directories, stale after 60 s): a refresh in flight either ends before it,
//! and the parked login is the refreshed one, or reads the new login after it.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};

use crate::account::{Account, Harness, validate_label};
use crate::secret;
use crate::store::Store;

/// Claude Code counts its refresh locks stale after 60 s, its config lock
/// after 10 s.
pub const REFRESH_STALE: Duration = Duration::from_secs(60);
const CONFIG_STALE: Duration = Duration::from_secs(10);
/// A refresh holds the lock for one request; longer means something is wrong.
const LOCK_WAIT: Duration = Duration::from_secs(10);
const SEAT_CHANGED: &str = "seat-changed";
/// How long sessions may still use the login that left: Claude Code keeps a
/// login it read for about 30 seconds.
pub const SETTLING: Duration = Duration::from_secs(60);

pub fn seat(store: &Store) -> Account {
    Account::default_for(Harness::Claude, store)
}

/// The account whose login sits in the seat: the account directory with the
/// same provider id, else the seat itself when no account holds that login.
pub fn current(store: &Store) -> Account {
    let seat = seat(store);
    let Some(id) = seat.provider_id(store).ok().flatten() else {
        return seat;
    };
    holder(store, &id).unwrap_or(seat)
}

/// The account directory signed in to `id`.
pub fn holder(store: &Store, id: &str) -> Option<Account> {
    Account::all(Harness::Claude, store)
        .ok()?
        .into_iter()
        .filter(|a| !a.is_default)
        .find(|a| a.provider_id(store).ok().flatten().as_deref() == Some(id))
}

/// When the seat last changed hands.
pub fn changed_at(store: &Store) -> Option<SystemTime> {
    fs::metadata(store.state.join(SEAT_CHANGED))
        .and_then(|m| m.modified())
        .ok()
}

/// Puts the login of `target` in the seat. Nothing happens when it is
/// already there.
pub fn sit(store: &Store, target: &Account) -> Result<(), String> {
    let seat = seat(store);
    let target_id = target
        .provider_id(store)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("'{}' is not signed in", target.label))?;
    let _refresh = Lock::take(seat.dir.join(".oauth_refresh.lock"), REFRESH_STALE)?;
    let _legacy = Lock::take(with_suffix(&seat.dir, ".lock"), REFRESH_STALE)?;
    let occupant = seat.login(store).map_err(|e| e.to_string())?;
    if occupant.as_ref().is_some_and(|o| o.id == target_id) {
        return Ok(());
    }
    // A renewal of the target's login (see `poll`) ends before it is read,
    // so the seat never gets a refresh token the provider already rotated.
    let _target = Lock::take(target.dir.join(".oauth_refresh.lock"), REFRESH_STALE)?;
    let _target_legacy = Lock::take(with_suffix(&target.dir, ".lock"), REFRESH_STALE)?;
    let incoming = secret::login(store, target)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "'{}' has no stored login; sign in again with `hats login {}`",
                target.label, target.label
            )
        })?;
    let leaving = secret::login(store, &seat).map_err(|e| e.to_string())?;
    let config_path = store.home.join(".claude.json");
    if let Some(leaving) = &leaving {
        let Some(occupant) = &occupant else {
            return Err(
                "~/.claude holds a login without an account in ~/.claude.json; \
                 sign in again with `claude /login` or run `claude auth logout`"
                    .to_owned(),
            );
        };
        let account = match holder(store, &occupant.id) {
            Some(account) => account,
            None => adopt(store, &config_path, occupant)?,
        };
        // A renewal of the copy it holds (see `poll`) must not land over it.
        let _parked = Lock::take(account.dir.join(".oauth_refresh.lock"), REFRESH_STALE)?;
        let _parked_legacy = Lock::take(with_suffix(&account.dir, ".lock"), REFRESH_STALE)?;
        secret::put_login(store, &account, leaving.clone()).map_err(|e| e.to_string())?;
    }
    secret::put_login(store, &seat, incoming).map_err(|e| e.to_string())?;
    let oauth_account = oauth_account(&target.dir.join(".claude.json"))?;
    let spliced = {
        let _config = Lock::take(with_suffix(&config_path, ".lock"), CONFIG_STALE)?;
        set_oauth_account(&config_path, oauth_account)
    };
    if let Err(e) = spliced {
        // The seat goes back to the login that matches its config.
        if let Some(leaving) = leaving {
            let _ = secret::put_login(store, &seat, leaving);
        }
        return Err(format!("could not update ~/.claude.json: {e}"));
    }
    store.ensure_state().map_err(|e| e.to_string())?;
    fs::write(store.state.join(SEAT_CHANGED), b"").map_err(|e| e.to_string())
}

/// An account directory for a login found only in the seat, named after its
/// mail, with that login's identity so hats and Claude Code recognize it.
fn adopt(
    store: &Store,
    config_path: &Path,
    occupant: &crate::account::Login,
) -> Result<Account, String> {
    let base = occupant
        .identity
        .email
        .as_deref()
        .and_then(|mail| mail.split('@').next())
        .map(|name| {
            name.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                        c
                    } else {
                        '-'
                    }
                })
                .take(28)
                .collect::<String>()
        })
        .filter(|name| validate_label(name).is_ok())
        .unwrap_or_else(|| "account".to_owned());
    let label = (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|label| !store.accounts_dir(Harness::Claude).join(label).exists())
        .expect("some label is free");
    let account = Account::managed(Harness::Claude, &label, store)?;
    account.create(store).map_err(|e| e.to_string())?;
    let identity = serde_json::json!({ "oauthAccount": oauth_account(config_path)? });
    write_private(&account.dir.join(".claude.json"), &identity.to_string())
        .map_err(|e| e.to_string())?;
    Ok(account)
}

pub fn oauth_account(config: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(config).map_err(|e| format!("{}: {e}", config.display()))?;
    let mut config: Map<String, Value> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", config.display()))?;
    config
        .remove("oauthAccount")
        .ok_or_else(|| "no oauthAccount in the account's .claude.json".to_owned())
}

/// Replaces `oauthAccount` and keeps every other key of Claude Code's config
/// in its place.
pub fn set_oauth_account(path: &Path, oauth_account: Value) -> io::Result<()> {
    let mut config: Map<String, Value> = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e),
    };
    config.insert("oauthAccount".to_owned(), oauth_account);
    let text = serde_json::to_string_pretty(&config).map_err(io::Error::other)?;
    write_private(path, &text)
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    let tmp = with_suffix(path, &format!(".{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(tmp, path)
}

pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// A `proper-lockfile` lock: a directory whose creation is the mutex, taken
/// over once its holder stopped touching it for `stale`.
pub struct Lock(PathBuf);

impl Lock {
    pub fn take(path: PathBuf, stale: Duration) -> Result<Self, String> {
        let started = SystemTime::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let age = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok());
                    if age.is_some_and(|age| age > stale) {
                        let _ = fs::remove_dir(&path);
                        continue;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Its parent is missing: no Claude Code ever ran there.
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    continue;
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
            if started.elapsed().unwrap_or_default() > LOCK_WAIT {
                return Err(format!(
                    "Claude Code held {} for over {} s; try again",
                    path.display(),
                    LOCK_WAIT.as_secs()
                ));
            }
            sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}
