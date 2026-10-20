//! Claude Code's stored login of one configuration directory, read and
//! written where Claude Code itself keeps it, so the seat can change hands
//! (see `seat`).
//!
//! On macOS that is a Keychain item, `Claude Code-credentials` for
//! `~/.claude` and `Claude Code-credentials-<first 8 hex digits of SHA-256 of
//! the directory>` for any other `CLAUDE_CONFIG_DIR` (measured on Claude Code
//! 2.1.304), under the user's name. Elsewhere, and on macOS for a HOME that is
//! not the user's own (tests), it is `<dir>/.credentials.json`.
//!
//! The item holds `claudeAiOauth` (the login) next to `mcpOAuth` (MCP servers
//! signed in from that directory). Only `claudeAiOauth` belongs to the
//! account; the rest stays with the directory.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::account::Account;
use crate::store::Store;

/// Apple's own binary, never one found on `PATH`: it handles the tokens.
const SECURITY: &str = "/usr/bin/security";
/// `errSecItemNotFound`, as `security` exits.
const NOT_FOUND: i32 = 44;
/// `security -i` reads each command with a 4096-byte buffer; a longer line is
/// cut and the write fails.
const STDIN_LINE: usize = 4096 - 64;
pub const LOGIN_KEY: &str = "claudeAiOauth";
const FILE_NAME: &str = ".credentials.json";

enum Backend {
    Keychain { service: String, user: String },
    File(PathBuf),
}

fn backend(store: &Store, account: &Account) -> Backend {
    if cfg!(target_os = "macos") && is_users_home(&store.home) {
        let service = if account.is_default {
            "Claude Code-credentials".to_owned()
        } else {
            let digest = Sha256::digest(account.dir.as_os_str().as_encoded_bytes());
            let suffix: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
            format!("Claude Code-credentials-{suffix}")
        };
        Backend::Keychain {
            service,
            user: user_name(),
        }
    } else {
        Backend::File(account.dir.join(FILE_NAME))
    }
}

/// The whole stored item, or `None` when the directory is not signed in.
pub fn read(store: &Store, account: &Account) -> io::Result<Option<Map<String, Value>>> {
    let text = match backend(store, account) {
        Backend::Keychain { service, user } => {
            let out = Command::new(SECURITY)
                .args(["find-generic-password", "-a", &user, "-w", "-s", &service])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()?;
            match out.status.code() {
                Some(0) => String::from_utf8(out.stdout).map_err(io::Error::other)?,
                Some(NOT_FOUND) => return Ok(None),
                code => {
                    return Err(io::Error::other(format!(
                        "the Keychain did not answer for {service} (security exited {code:?})"
                    )));
                }
            }
        }
        Backend::File(path) => match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        },
    };
    match serde_json::from_str(text.trim_end_matches('\n')) {
        Ok(Value::Object(item)) => Ok(Some(item)),
        _ => Err(io::Error::other("the stored login is not a JSON object")),
    }
}

/// The login alone, or `None` when the directory is not signed in.
pub fn login(store: &Store, account: &Account) -> io::Result<Option<Value>> {
    Ok(read(store, account)?.and_then(|mut item| item.remove(LOGIN_KEY)))
}

/// Puts `login` in the directory's item, keeping whatever else it holds.
pub fn put_login(store: &Store, account: &Account, login: Value) -> io::Result<()> {
    let mut item = read(store, account)?.unwrap_or_default();
    item.insert(LOGIN_KEY.to_owned(), login);
    write(store, account, &Value::Object(item).to_string())
}

fn write(store: &Store, account: &Account, text: &str) -> io::Result<()> {
    match backend(store, account) {
        Backend::Keychain { service, user } => {
            keychain_write(&service, &user, text)?;
            // Claude Code drops its cached token when this file changes, so a
            // running session takes the new login without waiting for its
            // Keychain cache to expire. Never created when absent.
            let file = account.dir.join(FILE_NAME);
            if file.exists() {
                write_file(&file, text)?;
            }
            Ok(())
        }
        Backend::File(path) => write_file(&path, text),
    }
}

/// `add-generic-password -U` through `security -i`, so the secret never shows
/// in the process list; hex (`-X`) needs no quoting.
fn keychain_write(service: &str, user: &str, text: &str) -> io::Result<()> {
    let hex: String = text.bytes().map(|b| format!("{b:02x}")).collect();
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let line = format!(
        "add-generic-password -U -a {} -s {} -X {hex}\n",
        quote(user),
        quote(service)
    );
    let status = if line.len() <= STDIN_LINE {
        let mut child = Command::new(SECURITY)
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .expect("piped")
            .write_all(line.as_bytes())?;
        child.wait()?
    } else {
        // Too long for `security -i`: the hex goes in the arguments.
        Command::new(SECURITY)
            .args(["add-generic-password", "-U", "-a", user, "-s", service])
            .args(["-X", &hex])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
    };
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the Keychain refused to save {service} ({status})"
        )))
    }
}

/// Replaced by rename, so a reader never sees half of it.
fn write_file(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
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

/// Claude Code names the Keychain item's account after `$USER`.
fn user_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| passwd().map(|(name, _)| name))
        .unwrap_or_else(|| "claude-code-user".to_owned())
}

fn is_users_home(home: &Path) -> bool {
    passwd().is_some_and(|(_, dir)| dir == home)
}

/// The user's name and home directory from the password database.
fn passwd() -> Option<(String, PathBuf)> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result = std::ptr::null_mut();
    // SAFETY: getpwuid_r fills `pwd` with pointers into `buf`, both alive here.
    let rc = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: on success `pwd` is initialized and its strings end in NUL.
    let pwd = unsafe { pwd.assume_init() };
    let (name, dir) = unsafe { (CStr::from_ptr(pwd.pw_name), CStr::from_ptr(pwd.pw_dir)) };
    Some((
        name.to_string_lossy().into_owned(),
        PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())),
    ))
}
