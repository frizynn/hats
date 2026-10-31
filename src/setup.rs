//! `hats setup`: installs, in one step and only when the user runs it, what
//! makes hats work without a resident process:
//!
//! - in `~/.claude/settings.json` (shared by every account): the
//!   `SessionStart`, `UserPromptSubmit`, `Stop` and `StopFailure` hooks, the
//!   `StopFailure` hook that wakes a cut session (see `hook::revive`), and the
//!   status line wrapper chained to the status line the user already has;
//! - in the shell's startup file: `claude` and `codex` functions that start
//!   through `hats run`.
//!
//! Every change is listed before writing, each file is backed up first, a
//! second run changes nothing, and `--undo` removes exactly what was added.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::shell_quote;
use crate::store::Store;

const RC_START: &str = "# >>> hats >>>";
const RC_END: &str = "# <<< hats <<<";
const BACKUP_TAG: &str = "hats-backup-";
/// One hook setup installs.
struct Hook {
    event: &'static str,
    /// What it runs after the binary: `'<path>/hats' hook stop`.
    args: &'static str,
    /// Only for these `error`s, run in the background, woken on exit 2.
    rewake: Option<&'static str>,
}

const HOOKS: [Hook; 5] = [
    Hook {
        event: "SessionStart",
        args: "hook session-start",
        rewake: None,
    },
    Hook {
        event: "UserPromptSubmit",
        args: "hook prompt",
        rewake: None,
    },
    Hook {
        event: "Stop",
        args: "hook stop",
        rewake: None,
    },
    Hook {
        event: "StopFailure",
        args: "hook stop",
        rewake: None,
    },
    Hook {
        event: "StopFailure",
        args: "hook revive",
        rewake: Some("rate_limit|authentication_failed"),
    },
];

/// How long a cut session may wait for an account: Claude Code ends an
/// `asyncRewake` hook after its `timeout`. A day covers a 5-hour window and
/// most of a weekly one.
const REVIVE_TIMEOUT_SECS: u64 = 24 * 3600;

impl Hook {
    /// The group setup writes for it in `settings.json`.
    fn group(&self, command: &str) -> Value {
        match self.rewake {
            None => json!({ "hooks": [{ "type": "command", "command": command }] }),
            Some(errors) => json!({
                "matcher": errors,
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "asyncRewake": true,
                    "timeout": REVIVE_TIMEOUT_SECS,
                }],
            }),
        }
    }
}

struct Plan {
    settings_path: PathBuf,
    settings: Value,
    settings_changed: bool,
    rc_path: PathBuf,
    rc: String,
    rc_changed: bool,
    changes: Vec<String>,
}

pub fn run(store: &Store, dry_run: bool, undo: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy().into_owned();
    let plan = if undo {
        plan_undo(store)?
    } else {
        plan_install(store, &exe)?
    };
    if plan.changes.is_empty() {
        println!(
            "{}",
            if undo {
                "Nothing to undo."
            } else {
                "Already set up; nothing changed."
            }
        );
        return Ok(());
    }
    println!(
        "{}",
        if dry_run {
            "Would change:"
        } else {
            "Changing:"
        }
    );
    for change in &plan.changes {
        println!("  {change}");
    }
    if dry_run {
        return Ok(());
    }
    let stamp = crate::limits::unix_secs(SystemTime::now());
    if plan.settings_changed {
        let mut text = serde_json::to_string_pretty(&plan.settings).expect("json value");
        text.push('\n');
        // Undoing to what a backup holds puts the user's own bytes back.
        let text = undo
            .then(|| {
                matching_backup(&plan.settings_path, |old| {
                    serde_json::from_str::<Value>(old).is_ok_and(|old| old == plan.settings)
                })
            })
            .flatten()
            .unwrap_or(text);
        write_with_backup(&plan.settings_path, &text, stamp)?;
    }
    if plan.rc_changed {
        let text = undo
            .then(|| {
                matching_backup(&plan.rc_path, |old| {
                    old.trim_end_matches('\n') == plan.rc.trim_end_matches('\n')
                })
            })
            .flatten()
            .unwrap_or(plan.rc);
        write_with_backup(&plan.rc_path, &text, stamp)?;
    }
    println!(
        "{}",
        if undo {
            "Done. Open a new terminal to drop the shell functions."
        } else {
            "Done. Open a new terminal, then `hats` shows your accounts."
        }
    );
    Ok(())
}

fn write_with_backup(path: &Path, text: &str, stamp: i64) -> Result<(), String> {
    let shown = |e: io::Error| format!("{}: {e}", path.display());
    let backup = backup(path, stamp).map_err(shown)?;
    write_atomic(path, text).map_err(shown)?;
    if let Some(b) = backup {
        println!("Backup: {}", b.display());
    }
    Ok(())
}

fn status_line_prefix(exe: &str) -> String {
    format!("{} statusline", shell_quote(exe))
}

fn shell_block(exe: &str, fish: bool) -> String {
    let q = shell_quote(exe);
    let functions = if fish {
        format!(
            "function claude; {q} run -- $argv; end\nfunction codex; {q} run --codex -- $argv; end"
        )
    } else {
        format!("claude() {{ {q} run -- \"$@\"; }}\ncodex() {{ {q} run --codex -- \"$@\"; }}")
    };
    format!(
        "{RC_START}\n# Added by `hats setup`; `hats setup --undo` removes it.\n{functions}\n{RC_END}\n"
    )
}

/// Where the shell block sits in `rc`, with the line break after it.
fn block_range(rc: &str) -> Option<Range<usize>> {
    let start = rc.find(RC_START)?;
    let end = start + rc[start..].find(RC_END)? + RC_END.len();
    let end = if rc[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };
    Some(start..end)
}

fn settings_path(store: &Store) -> PathBuf {
    store.home.join(".claude/settings.json")
}

/// Whether Claude Code runs the hooks and the status line that switch.
pub fn installed(store: &Store) -> bool {
    let Ok(text) = fs::read_to_string(settings_path(store)) else {
        return false;
    };
    ["hook stop\"", "statusline"]
        .iter()
        .all(|needle| text.contains(needle))
}

/// The interactive startup file of the user's shell.
fn rc_path(store: &Store) -> PathBuf {
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.ends_with("bash") {
        store.home.join(".bashrc")
    } else if shell.ends_with("fish") {
        store.home.join(".config/fish/conf.d/hats.fish")
    } else {
        store.home.join(".zshrc")
    }
}

fn load_settings(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{}: not valid JSON ({e}); fix it before `hats setup`",
                path.display()
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn read_text(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// What a command written by `setup` runs after the hats binary, wherever
/// that binary was when setup ran: `hook stop` for `'/old/path/hats' hook stop`.
fn hats_args(command: &str) -> Option<&str> {
    let (exe, rest) = unquote_word(command)?;
    let rest = rest.strip_prefix(' ')?;
    (Path::new(&exe).file_name()? == "hats").then_some(rest)
}

/// The single-quoted word at the start of `s`, as `shell_quote` writes it,
/// and what follows it.
fn unquote_word(s: &str) -> Option<(String, &str)> {
    let mut rest = s.strip_prefix('\'')?;
    let mut word = String::new();
    loop {
        let end = rest.find('\'')?;
        word.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        match rest.strip_prefix("\\''") {
            Some(after) => {
                word.push('\'');
                rest = after;
            }
            None => return Some((word, rest)),
        }
    }
}

/// The status line command setup's wrapper chains to: `Some(None)` when it
/// chains to nothing, `None` when `command` is not the wrapper.
fn chained_status_line(command: &str) -> Option<Option<String>> {
    let rest = hats_args(command)?.strip_prefix("statusline")?;
    if rest.is_empty() {
        return Some(None);
    }
    let (inner, tail) = unquote_word(rest.strip_prefix(" -- /bin/sh -c ")?)?;
    tail.is_empty().then_some(Some(inner))
}

/// Removes setup's hooks that run `args` from `groups`, and the groups that
/// held only them. Returns the removed commands.
fn remove_hooks(groups: &mut Vec<Value>, args: &str) -> Vec<String> {
    let mut removed = Vec::new();
    groups.retain_mut(|group| {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = hooks.len();
        hooks.retain(|hook| match hook["command"].as_str() {
            Some(command) if hats_args(command) == Some(args) => {
                removed.push(command.to_owned());
                false
            }
            _ => true,
        });
        !(hooks.is_empty() && hooks.len() < before)
    });
    removed
}

fn plan_install(store: &Store, exe: &str) -> Result<Plan, String> {
    let settings_path = settings_path(store);
    let mut settings = load_settings(&settings_path)?;
    let root = settings
        .as_object_mut()
        .ok_or("settings.json is not a JSON object")?;
    let mut changes = Vec::new();
    let shown = settings_path.display().to_string();

    let hooks_value = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks_map = hooks_value
        .as_object_mut()
        .ok_or("settings.json: \"hooks\" is not an object")?;
    for hook in &HOOKS {
        let (event, args) = (hook.event, hook.args);
        let command = format!("{} {args}", shell_quote(exe));
        let group = hook.group(&command);
        let groups = hooks_map.entry(event).or_insert_with(|| json!([]));
        let groups = groups
            .as_array_mut()
            .ok_or_else(|| format!("settings.json: hooks.{event} is not a list"))?;
        // Replaces hooks from an earlier setup, even of a binary that moved.
        let mut updated = groups.clone();
        let removed = remove_hooks(&mut updated, args);
        if removed == [command.as_str()] && groups.contains(&group) {
            continue;
        }
        updated.push(group);
        *groups = updated;
        changes.push(match removed.first() {
            None => format!("{shown}: add {event} hook `{command}`"),
            Some(old) => format!("{shown}: replace {event} hook `{old}` with `{command}`"),
        });
    }

    let prefix = status_line_prefix(exe);
    let current = root.get("statusLine").cloned();
    let current_command = current
        .as_ref()
        .and_then(|s| s["command"].as_str())
        .unwrap_or("")
        .to_owned();
    let chained = chained_status_line(&current_command);
    let user_command = match &chained {
        Some(inner) => inner.clone(),
        None => Some(current_command.clone()).filter(|c| !c.is_empty()),
    };
    let command = match &user_command {
        None => prefix,
        Some(user) => format!("{prefix} -- /bin/sh -c {}", shell_quote(user)),
    };
    if command != current_command {
        let mut wrapped = match current {
            Some(Value::Object(obj)) => obj,
            _ => Map::new(),
        };
        wrapped.insert("type".into(), "command".into());
        wrapped.insert("command".into(), command.clone().into());
        changes.push(match (&chained, current_command.as_str()) {
            (Some(_), old) => format!("{shown}: replace statusLine `{old}` with `{command}`"),
            (None, "") => format!("{shown}: set statusLine to `{command}`"),
            (None, old) => format!("{shown}: wrap statusLine `{old}` as `{command}`"),
        });
        root.insert("statusLine".into(), Value::Object(wrapped));
    }
    let settings_changed = !changes.is_empty();

    let rc_path = rc_path(store);
    let mut rc = read_text(&rc_path)?;
    let fish = rc_path.extension().is_some_and(|e| e == "fish");
    let block = shell_block(exe, fish);
    let rc_changed = match block_range(&rc) {
        Some(range) if rc[range.clone()] == block => false,
        Some(range) => {
            rc.replace_range(range, &block);
            changes.push(format!(
                "{}: point the `claude` and `codex` functions at {exe}",
                rc_path.display()
            ));
            true
        }
        None => {
            if !rc.is_empty() && !rc.ends_with('\n') {
                rc.push('\n');
            }
            rc.push_str(&block);
            changes.push(format!(
                "{}: add `claude` and `codex` functions that start through `hats run`",
                rc_path.display()
            ));
            true
        }
    };
    Ok(Plan {
        settings_path,
        settings,
        settings_changed,
        rc_path,
        rc,
        rc_changed,
        changes,
    })
}

fn plan_undo(store: &Store) -> Result<Plan, String> {
    let settings_path = settings_path(store);
    let mut settings = load_settings(&settings_path)?;
    let shown = settings_path.display().to_string();
    let mut changes = Vec::new();
    let root = settings
        .as_object_mut()
        .ok_or("settings.json is not a JSON object")?;

    if let Some(Value::Object(hooks_map)) = root.get_mut("hooks") {
        for &Hook { event, args, .. } in &HOOKS {
            let Some(Value::Array(groups)) = hooks_map.get_mut(event) else {
                continue;
            };
            let removed = remove_hooks(groups, args);
            if removed.is_empty() {
                continue;
            }
            for command in &removed {
                changes.push(format!("{shown}: remove {event} hook `{command}`"));
            }
            if groups.is_empty() {
                hooks_map.remove(event);
            }
        }
        if hooks_map.is_empty() && !changes.is_empty() {
            root.remove("hooks");
        }
    }

    let chained = root
        .get("statusLine")
        .and_then(|s| s["command"].as_str())
        .and_then(chained_status_line);
    match chained {
        Some(Some(user)) => {
            changes.push(format!("{shown}: restore statusLine `{user}`"));
            root["statusLine"]["command"] = user.into();
        }
        Some(None) => {
            changes.push(format!("{shown}: remove statusLine"));
            root.remove("statusLine");
        }
        None => {}
    }
    let settings_changed = !changes.is_empty();

    let rc_path = rc_path(store);
    let mut rc = read_text(&rc_path)?;
    let rc_changed = match block_range(&rc) {
        Some(range) => {
            rc.replace_range(range, "");
            changes.push(format!(
                "{}: remove the hats shell functions",
                rc_path.display()
            ));
            true
        }
        None => false,
    };
    Ok(Plan {
        settings_path,
        settings,
        settings_changed,
        rc_path,
        rc,
        rc_changed,
        changes,
    })
}

/// Copies `path` next to itself, never over an earlier backup; `None` if it
/// did not exist. The copy is no more readable than the original.
fn backup(path: &Path, stamp: i64) -> io::Result<Option<PathBuf>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mode = fs::metadata(path)?.permissions().mode();
    let base = format!(
        "{}.{BACKUP_TAG}{stamp}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    for n in 0u32.. {
        let name = if n == 0 {
            base.clone()
        } else {
            format!("{base}.{n}")
        };
        let backup = path.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&backup)
        {
            Ok(mut file) => {
                file.write_all(&bytes)?;
                return Ok(Some(backup));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("some backup name is free")
}

/// The newest backup of `path` that `same` accepts.
fn matching_backup(path: &Path, same: impl Fn(&str) -> bool) -> Option<String> {
    let dir = path.parent()?;
    let prefix = format!("{}.{BACKUP_TAG}", path.file_name()?.to_str()?);
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix))
        .collect();
    names.sort();
    names
        .iter()
        .rev()
        .filter_map(|name| fs::read_to_string(dir.join(name)).ok())
        .find(|text| same(text))
}

/// Writes through a symlink to its target, so a linked settings.json stays linked.
fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("hats-tmp");
    fs::write(&tmp, text)?;
    if let Ok(meta) = fs::metadata(&path) {
        fs::set_permissions(&tmp, meta.permissions())?;
    }
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_its_commands_wherever_the_binary_was() {
        let exe = "/home/o'neil/bin/hats";
        let hook = format!("{} hook stop", shell_quote(exe));
        assert_eq!(hats_args(&hook), Some("hook stop"));
        assert_eq!(hats_args("'/opt/other/tool' hook stop"), None);
        assert_eq!(hats_args("hats hook stop"), None);

        let wrapped = format!(
            "{} -- /bin/sh -c {}",
            status_line_prefix(exe),
            shell_quote("printf '%s' \"$x\" | head -1")
        );
        assert_eq!(
            chained_status_line(&wrapped),
            Some(Some("printf '%s' \"$x\" | head -1".to_owned()))
        );
        assert_eq!(chained_status_line(&status_line_prefix(exe)), Some(None));
        assert_eq!(chained_status_line("bun status.ts"), None);
    }
}
