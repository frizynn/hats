mod common;

use std::fs;
use std::io::Write;
use std::process::Stdio;

use common::{Sandbox, mode};

fn hook(sb: &Sandbox, config_dir: Option<&std::path::Path>, input: &str) -> std::process::Output {
    let mut cmd = sb.cmd(&["hook", "session-start"]);
    if let Some(dir) = config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn add_shares_settings_and_history_by_symlink() {
    let sb = Sandbox::new();
    let claude = sb.home.join(".claude");
    fs::write(claude.join("settings.json"), "{}").unwrap();
    fs::create_dir_all(claude.join("skills/demo")).unwrap();

    sb.ok(&["add", "work"]);

    let dir = sb.data().join("claude/work");
    assert_eq!(mode(&dir), 0o700);
    for (name, target) in [
        ("settings.json", claude.join("settings.json")),
        ("skills", claude.join("skills")),
        ("projects", claude.join("projects")),
    ] {
        assert_eq!(fs::read_link(dir.join(name)).unwrap(), target, "{name}");
    }
    // Entries the user does not have are not invented, except shared history.
    assert!(fs::symlink_metadata(dir.join("CLAUDE.md")).is_err());
    assert!(claude.join("projects").is_dir());
    // The login is never shared.
    assert!(fs::symlink_metadata(dir.join(".claude.json")).is_err());
}

#[test]
fn add_rejects_bad_and_duplicate_labels() {
    let sb = Sandbox::new();
    for bad in ["default", "../x", "-x"] {
        let out = sb.cmd(&["add", bad]).output().unwrap();
        assert!(!out.status.success(), "{bad}");
    }
    sb.ok(&["add", "work"]);
    assert!(!sb.cmd(&["add", "work"]).output().unwrap().status.success());
}

#[test]
fn run_execs_the_harness_with_the_account_directory() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    let dir = sb.data().join("claude/work");

    let out = sb.ok(&["run", "work", "--", "--resume", "abc"]);
    assert_eq!(
        out.trim(),
        format!(
            "claude CLAUDE_CONFIG_DIR={} args=--resume abc",
            dir.display()
        )
    );

    // The default account starts with the variable unset, even if the
    // caller's shell had one exported.
    let out = sb
        .cmd(&["run", "default"])
        .env("CLAUDE_CONFIG_DIR", "/elsewhere")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "claude CLAUDE_CONFIG_DIR=<unset> args="
    );
}

#[test]
fn run_without_label_uses_the_current_account() {
    let sb = Sandbox::new();
    assert!(sb.ok(&["run"]).contains("CLAUDE_CONFIG_DIR=<unset>"));

    sb.ok(&["add", "work"]);
    sb.ok(&["login", "work"]);
    sb.ok(&["use", "work"]);
    let dir = sb.data().join("claude/work");
    assert!(
        sb.ok(&["run"])
            .contains(&format!("CLAUDE_CONFIG_DIR={}", dir.display()))
    );
    assert_eq!(
        sb.ok(&["env"]).trim(),
        format!("export CLAUDE_CONFIG_DIR='{}'", dir.display())
    );
}

#[test]
fn use_refuses_an_account_that_is_not_logged_in() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    let out = sb.cmd(&["use", "work"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not logged in"));
    assert!(sb.registry_lines().is_empty());
}

#[test]
fn login_runs_the_harness_login_inside_the_account() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    assert_eq!(sb.ok(&["login", "work"]).trim(), "logged in");
    let list = sb.ok(&["list"]);
    let row = list.lines().find(|l| l.contains("work")).unwrap();
    assert!(row.contains("9ae25bfbd05c9e76"), "{list}");
    // The mail stays inside the harness's file.
    assert!(!list.contains("example.com"), "{list}");
}

#[test]
fn list_marks_the_current_account() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    sb.ok(&["add", "personal"]);
    sb.ok(&["login", "work"]);
    let list = sb.ok(&["list"]);
    let labels: Vec<_> = list
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(labels, ["*default", "personal", "work"], "{list}");
    assert!(list.contains("(not logged in)"));

    sb.ok(&["use", "work"]);
    assert!(sb.ok(&["list"]).lines().any(|l| l.contains("*work")));
}

#[test]
fn remove_signs_out_and_keeps_shared_data() {
    let sb = Sandbox::new();
    let claude = sb.home.join(".claude");
    fs::write(claude.join("settings.json"), "{\"keep\":true}").unwrap();
    fs::create_dir_all(claude.join("projects/repo")).unwrap();
    fs::write(claude.join("projects/repo/s.jsonl"), "{}\n").unwrap();
    sb.ok(&["add", "work"]);
    sb.ok(&["login", "work"]);
    sb.ok(&["use", "work"]);

    let out = sb.ok(&["remove", "work"]);
    assert!(out.contains("logged out"), "harness sign-out ran: {out}");
    assert!(!sb.data().join("claude/work").exists());
    assert_eq!(
        fs::read_to_string(claude.join("settings.json")).unwrap(),
        "{\"keep\":true}"
    );
    assert!(claude.join("projects/repo/s.jsonl").exists());
    // The current account falls back to the default one.
    assert!(sb.ok(&["run"]).contains("CLAUDE_CONFIG_DIR=<unset>"));
    assert!(
        !sb.cmd(&["remove", "default"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn hook_attributes_the_session_to_the_running_account() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    sb.ok(&["login", "work"]);
    let dir = sb.data().join("claude/work");

    let out = hook(&sb, Some(&dir), r#"{"session_id":"s-1","source":"resume"}"#);
    assert!(out.status.success() && out.stdout.is_empty());
    let lines = sb.registry_lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["label"], "work");
    assert_eq!(lines[0]["session"], "s-1");
    assert_eq!(lines[0]["configDir"], dir.display().to_string());
}

#[test]
fn hook_skips_compaction_and_logged_out_accounts() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    let dir = sb.data().join("claude/work");
    let out = hook(
        &sb,
        Some(&dir),
        r#"{"session_id":"s-1","source":"startup"}"#,
    );
    assert!(out.status.success());

    sb.ok(&["login", "work"]);
    let out = hook(
        &sb,
        Some(&dir),
        r#"{"session_id":"s-1","source":"compact"}"#,
    );
    assert!(out.status.success());
    assert!(sb.registry_lines().is_empty());
}

#[test]
fn hook_never_fails_the_session() {
    let sb = Sandbox::new();
    for input in ["", "not json", r#"{"source":"startup"}"#] {
        let out = hook(&sb, None, input);
        assert!(out.status.success(), "{input}");
        assert!(out.stdout.is_empty(), "{input}");
    }
}

#[test]
fn concurrent_hooks_never_interleave_lines() {
    let sb = Sandbox::new();
    sb.login_claude(&sb.home.join(".claude.json"));
    let children: Vec<_> = (0..32)
        .map(|i| {
            let mut child = sb
                .cmd(&["hook", "session-start"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let input = format!(r#"{{"session_id":"s-{i}","source":"startup"}}"#);
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let mut sessions: Vec<_> = sb
        .registry_lines()
        .iter()
        .map(|l| l["session"].as_str().unwrap().to_owned())
        .collect();
    sessions.sort();
    let mut expected: Vec<_> = (0..32).map(|i| format!("s-{i}")).collect();
    expected.sort();
    assert_eq!(sessions, expected);
}

#[test]
fn codex_accounts_use_codex_home_and_rollout_identity() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.home.join(".codex")).unwrap();
    fs::write(sb.home.join(".codex/config.toml"), "model = \"demo\"\n").unwrap();

    sb.ok(&["add", "--codex", "personal"]);
    let dir = sb.data().join("codex/personal");
    assert_eq!(
        fs::read_link(dir.join("config.toml")).unwrap(),
        sb.home.join(".codex/config.toml")
    );
    assert!(
        fs::symlink_metadata(dir.join("sessions")).is_err(),
        "sessions stay per account"
    );

    let out = sb.ok(&["run", "--codex", "personal", "--", "resume"]);
    assert_eq!(
        out.trim(),
        format!("codex CODEX_HOME={} args=resume", dir.display())
    );

    // Codex names the account in each rollout header; the newest one wins.
    for (day, id) in [
        ("01", "acct-old"),
        ("02", "acct-00000000-0000-4000-8000-0000000000cd"),
    ] {
        let day_dir = dir.join(format!("sessions/2026/01/{day}"));
        fs::create_dir_all(&day_dir).unwrap();
        fs::write(
            day_dir.join(format!("rollout-2026-01-{day}T10-00-00-x.jsonl")),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"creator_account_id\":\"{id}\",\"id\":\"x\"}}}}\n{{\"type\":\"event_msg\"}}\n"
            ),
        )
        .unwrap();
    }
    sb.ok(&["use", "--codex", "personal"]);
    let lines = sb.registry_lines();
    assert_eq!(lines[0]["harness"], "codex");
    assert_eq!(lines[0]["account"], "9a00cf1657c91b7f");
}

#[test]
fn state_paths_follow_overrides() {
    let sb = Sandbox::new();
    let alt = sb.home.join("alt");
    let out = sb
        .cmd(&["add", "work"])
        .env("AGENT_ACCOUNTS_HOME", alt.join("data"))
        .env("XDG_STATE_HOME", alt.join("xdg-state"))
        .output()
        .unwrap();
    common::assert_success(&out, &["add", "work"]);
    assert!(alt.join("data/claude/work").is_dir());
}

#[test]
fn setup_only_prints() {
    let sb = Sandbox::new();
    let settings = sb.home.join(".claude/settings.json");
    fs::write(&settings, "{}").unwrap();
    let out = sb.ok(&["setup"]);
    assert!(out.contains("\"SessionStart\""));
    assert!(out.contains("hook session-start"));
    assert_eq!(fs::read_to_string(settings).unwrap(), "{}");
}

#[test]
fn usage_errors_exit_2() {
    let sb = Sandbox::new();
    for args in [
        &["frobnicate"][..],
        &["add"],
        &["run", "a", "b"],
        &["--bogus"],
    ] {
        let out = sb.cmd(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
}
