//! `hats setup` changes the user's files only when run, shows each change,
//! keeps a backup, does nothing the second time, and `--undo` restores.

mod common;

use std::fs;

use common::Sandbox;

const ORIGINAL: &str = r#"{
  "model": "opus",
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "echo mine" }] }
    ]
  },
  "statusLine": { "type": "command", "command": "bun /opt/demo/status.ts | head -1", "padding": 0 }
}
"#;

fn setup_sandbox() -> (Sandbox, std::path::PathBuf, std::path::PathBuf) {
    let sb = Sandbox::new();
    let settings = sb.home.join(".claude/settings.json");
    fs::write(&settings, ORIGINAL).unwrap();
    let rc = sb.home.join(".zshrc");
    fs::write(&rc, "export EDITOR=vi\n").unwrap();
    (sb, settings, rc)
}

fn json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn installs_hooks_chained_status_line_and_shell_functions() {
    let (sb, settings, rc) = setup_sandbox();
    let out = sb
        .cmd(&["setup"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    common::assert_success(&out, &["setup"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("add SessionStart hook"), "{stdout}");
    assert!(
        stdout.contains("wrap statusLine `bun /opt/demo/status.ts | head -1`"),
        "{stdout}"
    );
    assert!(stdout.contains("Backup:"), "{stdout}");

    let v = json(&settings);
    assert_eq!(v["model"], "opus", "other settings kept");
    let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["model", "hooks", "statusLine"], "key order kept");
    let start = v["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(start.len(), 2);
    assert_eq!(
        start[0]["hooks"][0]["command"], "echo mine",
        "user's hook kept first"
    );
    assert!(
        start[1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("hook session-start")
    );
    for event in ["UserPromptSubmit", "Stop", "StopFailure"] {
        assert_eq!(v["hooks"][event].as_array().unwrap().len(), 1, "{event}");
    }
    let status = v["statusLine"]["command"].as_str().unwrap();
    assert!(
        status.ends_with("statusline -- /bin/sh -c 'bun /opt/demo/status.ts | head -1'"),
        "{status}"
    );
    assert_eq!(v["statusLine"]["padding"], 0, "status line options kept");

    let rc_text = fs::read_to_string(&rc).unwrap();
    assert!(
        rc_text.starts_with("export EDITOR=vi\n# >>> hats >>>"),
        "{rc_text}"
    );
    assert!(
        rc_text.contains("claude() {") && rc_text.contains("run --codex -- \"$@\""),
        "{rc_text}"
    );

    let backups: Vec<_> = fs::read_dir(sb.home.join(".claude"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("settings.json.hats-backup-"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        fs::read_to_string(sb.home.join(".claude").join(&backups[0])).unwrap(),
        ORIGINAL
    );
}

#[test]
fn second_run_changes_nothing() {
    let (sb, settings, rc) = setup_sandbox();
    sb.cmd(&["setup"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    let (s1, r1) = (
        fs::read_to_string(&settings).unwrap(),
        fs::read_to_string(&rc).unwrap(),
    );
    let out = sb
        .cmd(&["setup"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("Already set up"));
    assert_eq!(fs::read_to_string(&settings).unwrap(), s1);
    assert_eq!(fs::read_to_string(&rc).unwrap(), r1);
}

#[test]
fn undo_restores_the_users_files() {
    let (sb, settings, rc) = setup_sandbox();
    sb.cmd(&["setup"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    let out = sb
        .cmd(&["setup", "--undo"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    common::assert_success(&out, &["setup", "--undo"]);
    assert_eq!(
        json(&settings),
        serde_json::from_str::<serde_json::Value>(ORIGINAL).unwrap()
    );
    assert_eq!(fs::read_to_string(&rc).unwrap(), "export EDITOR=vi\n");
    let out = sb
        .cmd(&["setup", "--undo"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("Nothing to undo"));
}

#[test]
fn dry_run_writes_nothing() {
    let (sb, settings, rc) = setup_sandbox();
    let out = sb
        .cmd(&["setup", "--dry-run"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("Would change:"));
    assert_eq!(fs::read_to_string(&settings).unwrap(), ORIGINAL);
    assert_eq!(fs::read_to_string(&rc).unwrap(), "export EDITOR=vi\n");
}

#[test]
fn works_from_scratch_and_keeps_a_linked_settings_file_linked() {
    let sb = Sandbox::new();
    let real = sb.home.join("dotfiles/settings.json");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, sb.home.join(".claude/settings.json")).unwrap();
    let out = sb
        .cmd(&["setup"])
        .env("SHELL", "/bin/bash")
        .output()
        .unwrap();
    common::assert_success(&out, &["setup"]);
    assert!(
        fs::symlink_metadata(sb.home.join(".claude/settings.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let v = json(&real);
    assert!(
        v["statusLine"]["command"]
            .as_str()
            .unwrap()
            .ends_with("statusline")
    );
    assert!(
        fs::read_to_string(sb.home.join(".bashrc"))
            .unwrap()
            .contains("# >>> hats >>>")
    );

    sb.cmd(&["setup", "--undo"])
        .env("SHELL", "/bin/bash")
        .output()
        .unwrap();
    assert_eq!(json(&real), serde_json::json!({}));
}

#[test]
fn refuses_invalid_settings_without_touching_them() {
    let sb = Sandbox::new();
    let settings = sb.home.join(".claude/settings.json");
    fs::write(&settings, "{ not json").unwrap();
    let out = sb
        .cmd(&["setup"])
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not valid JSON"));
    assert_eq!(fs::read_to_string(settings).unwrap(), "{ not json");
}

fn setup(sb: &Sandbox, args: &[&str]) -> String {
    let out = sb.cmd(args).env("SHELL", "/bin/zsh").output().unwrap();
    common::assert_success(&out, args);
    String::from_utf8(out.stdout).unwrap()
}

fn backups(sb: &Sandbox) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(sb.home.join(".claude"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("settings.json.hats-backup-"))
        .collect();
    names.sort();
    names
}

#[test]
fn undo_puts_back_the_exact_bytes() {
    let sb = Sandbox::new();
    let settings = sb.home.join(".claude/settings.json");
    // Not the layout serde_json writes: four spaces, inline arrays, no final newline.
    let original = "{\n    \"model\": \"opus\",\n    \"env\": {\"A\": \"1\"},\n    \"permissions\": {\"allow\": [\"Bash(ls)\", \"Read\"]},\n    \"cleanupPeriodDays\": 1.0\n}";
    fs::write(&settings, original).unwrap();
    let rc = sb.home.join(".zshrc");
    fs::write(&rc, "export EDITOR=vi").unwrap();

    setup(&sb, &["setup"]);
    setup(&sb, &["setup", "--undo"]);

    assert_eq!(fs::read_to_string(&settings).unwrap(), original);
    assert_eq!(fs::read_to_string(&rc).unwrap(), "export EDITOR=vi");
}

#[test]
fn a_backup_is_never_overwritten() {
    let (sb, settings, _) = setup_sandbox();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Whatever second setup runs in, a backup with that name already exists.
    for stamp in now - 2..now + 60 {
        fs::write(
            sb.home
                .join(format!(".claude/settings.json.hats-backup-{stamp}")),
            format!("older {stamp}"),
        )
        .unwrap();
    }
    setup(&sb, &["setup"]);
    setup(&sb, &["setup", "--undo"]);

    for stamp in now - 2..now + 60 {
        let path = sb
            .home
            .join(format!(".claude/settings.json.hats-backup-{stamp}"));
        assert_eq!(fs::read_to_string(path).unwrap(), format!("older {stamp}"));
    }
    let originals = backups(&sb)
        .iter()
        .filter(|n| fs::read_to_string(sb.home.join(".claude").join(n)).unwrap() == ORIGINAL)
        .count();
    assert_eq!(originals, 1, "{:?}", backups(&sb));
    assert_eq!(fs::read_to_string(settings).unwrap(), ORIGINAL);
}

#[test]
fn undo_restores_the_status_line_without_its_own_state() {
    let (sb, settings, _) = setup_sandbox();
    setup(&sb, &["setup"]);
    // Whatever hats keeps elsewhere may be gone by the time the user undoes.
    let _ = fs::remove_dir_all(sb.home.join(".local/state/hats"));
    setup(&sb, &["setup", "--undo"]);
    assert_eq!(
        json(&settings)["statusLine"]["command"],
        "bun /opt/demo/status.ts | head -1"
    );
}

#[test]
fn setup_from_a_moved_binary_updates_its_entries() {
    let (sb, settings, rc) = setup_sandbox();
    setup(&sb, &["setup"]);
    let moved = sb.home.join("bin2/hats");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    // Copied by a child process, for the reason in `common::write_program`.
    let copied = std::process::Command::new("cp")
        .arg(env!("CARGO_BIN_EXE_hats"))
        .arg(&moved)
        .status()
        .unwrap();
    assert!(copied.success());
    let run_moved = |args: &[&str]| {
        // The sandbox environment, with the moved copy as the program.
        let cmd = sb.cmd(args);
        let out = std::process::Command::new(&moved)
            .args(args)
            .env_clear()
            .envs(cmd.get_envs().filter_map(|(k, v)| Some((k, v?))))
            .env("SHELL", "/bin/zsh")
            .output()
            .unwrap();
        common::assert_success(&out, args);
    };
    run_moved(&["setup"]);

    let v = json(&settings);
    let quoted = format!("'{}'", moved.display());
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "StopFailure"] {
        let commands: Vec<&str> = v["hooks"][event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .map(|h| h["command"].as_str().unwrap())
            .filter(|c| c.contains("hats"))
            .collect();
        assert_eq!(commands.len(), 1, "{event}: {commands:?}");
        assert!(commands[0].starts_with(&quoted), "{event}: {commands:?}");
    }
    assert_eq!(
        v["statusLine"]["command"],
        format!("{quoted} statusline -- /bin/sh -c 'bun /opt/demo/status.ts | head -1'")
    );
    let rc_text = fs::read_to_string(&rc).unwrap();
    assert_eq!(rc_text.matches("# >>> hats >>>").count(), 1, "{rc_text}");
    assert!(
        rc_text.contains(&format!("claude() {{ {quoted} run")),
        "{rc_text}"
    );

    run_moved(&["setup", "--undo"]);
    assert_eq!(fs::read_to_string(&settings).unwrap(), ORIGINAL);
    assert_eq!(fs::read_to_string(&rc).unwrap(), "export EDITOR=vi\n");
}
