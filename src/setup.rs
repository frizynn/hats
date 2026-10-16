//! `hats setup`: installs, in one step and only when the user runs it, what
//! makes hats work without a resident process:
//!
//! - in `~/.claude/settings.json` (shared by every account): the
//!   `SessionStart`, `UserPromptSubmit`, `Stop` and `StopFailure` hooks, and
//!   the status line wrapper chained to the status line the user already has;
//! - in the shell's startup file: `claude` and `codex` functions that start
//!   through `hats run`.
//!
//! Every change is listed before writing, each file is backed up first, a
//! second run changes nothing, and `--undo` removes exactly what was added.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::store::Store;

const RC_START: &str = "# >>> hats >>>";
const RC_END: &str = "# <<< hats <<<";
const RECORD: &str = "setup.json";

/// What setup replaced, so `--undo` can put it back.
#[derive(Serialize, Deserialize, Default)]
struct Record {
    /// The user's status line before it was wrapped; `null` if there was none.
    status_line: Option<Value>,
}

struct Plan {
    settings_path: PathBuf,
    settings: Value,
    settings_changed: bool,
    rc_path: PathBuf,
    rc: String,
    rc_changed: bool,
    changes: Vec<String>,
    record: Record,
}

pub fn run(store: &Store, dry_run: bool, undo: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy().into_owned();
    let plan = if undo {
        plan_undo(store, &exe)?
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
        let backup = backup(&plan.settings_path, stamp).map_err(|e| e.to_string())?;
        let mut text = serde_json::to_string_pretty(&plan.settings).expect("json value");
        text.push('\n');
        write_atomic(&plan.settings_path, &text)
            .map_err(|e| format!("{}: {e}", plan.settings_path.display()))?;
        if let Some(b) = backup {
            println!("Backup: {}", b.display());
        }
    }
    if plan.rc_changed {
        let backup = backup(&plan.rc_path, stamp).map_err(|e| e.to_string())?;
        write_atomic(&plan.rc_path, &plan.rc)
            .map_err(|e| format!("{}: {e}", plan.rc_path.display()))?;
        if let Some(b) = backup {
            println!("Backup: {}", b.display());
        }
    }
    store.ensure_state().map_err(|e| e.to_string())?;
    let record_path = store.state.join(RECORD);
    if undo {
        let _ = fs::remove_file(record_path);
    } else {
        let bytes = serde_json::to_vec_pretty(&plan.record).expect("plain struct");
        fs::write(record_path, bytes).map_err(|e| e.to_string())?;
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

fn hooks(exe: &str) -> [(&'static str, String); 4] {
    let q = crate::shell_quote(exe);
    [
        ("SessionStart", format!("{q} hook session-start")),
        ("UserPromptSubmit", format!("{q} hook prompt")),
        ("Stop", format!("{q} hook stop")),
        ("StopFailure", format!("{q} hook stop")),
    ]
}

fn status_line_prefix(exe: &str) -> String {
    format!("{} statusline", crate::shell_quote(exe))
}

fn shell_block(exe: &str, fish: bool) -> String {
    let q = crate::shell_quote(exe);
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

fn settings_path(store: &Store) -> PathBuf {
    store.home.join(".claude/settings.json")
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

fn is_ours(group: &Value, command: &str) -> bool {
    group["hooks"]
        .as_array()
        .is_some_and(|hooks| hooks.iter().any(|h| h["command"] == command))
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
    for (event, command) in hooks(exe) {
        let groups = hooks_map.entry(event).or_insert_with(|| json!([]));
        let groups = groups
            .as_array_mut()
            .ok_or_else(|| format!("settings.json: hooks.{event} is not a list"))?;
        if !groups.iter().any(|g| is_ours(g, &command)) {
            groups.push(json!({ "hooks": [{ "type": "command", "command": command }] }));
            changes.push(format!("{shown}: add {event} hook `{command}`"));
        }
    }

    let prefix = status_line_prefix(exe);
    let current = root.get("statusLine").cloned();
    let current_command = current
        .as_ref()
        .and_then(|s| s["command"].as_str())
        .unwrap_or("");
    let mut record = Record::default();
    if !current_command.starts_with(&prefix) {
        let mut wrapped = match &current {
            Some(Value::Object(obj)) => obj.clone(),
            _ => Map::new(),
        };
        let command = if current_command.is_empty() {
            prefix.clone()
        } else {
            format!(
                "{prefix} -- /bin/sh -c {}",
                crate::shell_quote(current_command)
            )
        };
        wrapped.insert("type".into(), "command".into());
        wrapped.insert("command".into(), command.clone().into());
        changes.push(match current_command {
            "" => format!("{shown}: set statusLine to `{command}`"),
            old => format!("{shown}: wrap statusLine `{old}` as `{command}`"),
        });
        record.status_line = current;
        root.insert("statusLine".into(), Value::Object(wrapped));
    } else if let Ok(bytes) = fs::read(store.state.join(RECORD)) {
        record = serde_json::from_slice(&bytes).unwrap_or_default();
    }
    let settings_changed = !changes.is_empty();

    let rc_path = rc_path(store);
    let mut rc = read_text(&rc_path)?;
    let rc_changed = !rc.contains(RC_START);
    if rc_changed {
        if !rc.is_empty() && !rc.ends_with('\n') {
            rc.push('\n');
        }
        let fish = rc_path.extension().is_some_and(|e| e == "fish");
        rc.push_str(&shell_block(exe, fish));
        changes.push(format!(
            "{}: add `claude` and `codex` functions that start through `hats run`",
            rc_path.display()
        ));
    }
    Ok(Plan {
        settings_path,
        settings,
        settings_changed,
        rc_path,
        rc,
        rc_changed,
        changes,
        record,
    })
}

fn plan_undo(store: &Store, exe: &str) -> Result<Plan, String> {
    let settings_path = settings_path(store);
    let mut settings = load_settings(&settings_path)?;
    let shown = settings_path.display().to_string();
    let mut changes = Vec::new();
    let root = settings
        .as_object_mut()
        .ok_or("settings.json is not a JSON object")?;

    if let Some(Value::Object(hooks_map)) = root.get_mut("hooks") {
        for (event, command) in hooks(exe) {
            let Some(Value::Array(groups)) = hooks_map.get_mut(event) else {
                continue;
            };
            let before = groups.len();
            groups.retain(|g| !is_ours(g, &command));
            if groups.len() != before {
                changes.push(format!("{shown}: remove {event} hook `{command}`"));
            }
            if groups.is_empty() {
                hooks_map.remove(event);
            }
        }
        if hooks_map.is_empty() {
            root.remove("hooks");
        }
    }

    let prefix = status_line_prefix(exe);
    let wrapped = root
        .get("statusLine")
        .and_then(|s| s["command"].as_str())
        .is_some_and(|c| c.starts_with(&prefix));
    if wrapped {
        let record: Record = fs::read(store.state.join(RECORD))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        match record.status_line {
            Some(original) => {
                changes.push(format!(
                    "{shown}: restore statusLine `{}`",
                    original["command"].as_str().unwrap_or("")
                ));
                root.insert("statusLine".into(), original);
            }
            None => {
                changes.push(format!("{shown}: remove statusLine"));
                root.remove("statusLine");
            }
        }
    }
    let settings_changed = !changes.is_empty();

    let rc_path = rc_path(store);
    let mut rc = read_text(&rc_path)?;
    let rc_changed = match (rc.find(RC_START), rc.find(RC_END)) {
        (Some(start), Some(end)) if end > start => {
            let end = end + RC_END.len();
            let end = if rc[end..].starts_with('\n') {
                end + 1
            } else {
                end
            };
            rc.replace_range(start..end, "");
            changes.push(format!(
                "{}: remove the hats shell functions",
                rc_path.display()
            ));
            true
        }
        _ => false,
    };
    Ok(Plan {
        settings_path,
        settings,
        settings_changed,
        rc_path,
        rc,
        rc_changed,
        changes,
        record: Record::default(),
    })
}

/// Copies `path` next to itself before the first change; `None` if it did not exist.
fn backup(path: &Path, stamp: i64) -> io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let name = format!(
        "{}.hats-backup-{stamp}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let backup = path.with_file_name(name);
    fs::copy(path, &backup)?;
    Ok(Some(backup))
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
