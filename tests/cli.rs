mod common;

use std::fs;
use std::io::Write;
use std::process::Stdio;

use common::{Sandbox, mode};

fn account(list_json: &str, label: &str) -> serde_json::Value {
    let doc: serde_json::Value = serde_json::from_str(list_json).unwrap();
    doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == label)
        .unwrap()
        .clone()
}

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
fn run_links_shared_entries_created_after_add() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    let claude = sb.home.join(".claude");
    fs::write(claude.join("CLAUDE.md"), "# rules\n").unwrap();

    sb.ok(&["run", "work"]);

    let dir = sb.data().join("claude/work");
    assert_eq!(
        fs::read_link(dir.join("CLAUDE.md")).unwrap(),
        claude.join("CLAUDE.md")
    );
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
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("not signed in") && err.contains("hats login work"),
        "{err}"
    );
    assert!(sb.registry_lines().is_empty());
}

#[test]
fn login_runs_the_harness_login_inside_the_account() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    assert_eq!(sb.ok(&["login", "work"]).trim(), "logged in");
    let list = sb.ok(&["list", "--json"]);
    let work = account(&list, "work");
    assert_eq!(work["account"], "9ae25bfbd05c9e76");
    assert_eq!(work["signedIn"], true);
    // The mail is shown to tell accounts apart, but never kept in hats' files.
    assert_eq!(work["email"], "user@example.com");
    assert!(sb.ok(&["list"]).contains("user@example.com"));
    sb.ok(&["use", "work"]);
    let registry = fs::read_to_string(sb.registry()).unwrap();
    assert!(!registry.contains('@'), "{registry}");
}

#[test]
fn list_marks_the_current_account() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    sb.ok(&["add", "personal"]);
    sb.ok(&["login", "work"]);
    let list = sb.ok(&["list", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(doc["v"], 1);
    let labels: Vec<_> = doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["default", "personal", "work"], "{list}");
    assert_eq!(account(&list, "default")["current"], true);
    assert_eq!(account(&list, "personal")["signedIn"], false);

    sb.ok(&["use", "work"]);
    let list = sb.ok(&["list", "--json"]);
    assert_eq!(account(&list, "work")["current"], true);
    assert_eq!(account(&list, "default")["current"], false);
    let text = sb.ok(&["list"]);
    assert!(text.lines().any(|l| l.starts_with("● work")), "{text}");
    assert!(
        text.contains("not signed in: hats login personal"),
        "{text}"
    );
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
fn remove_never_follows_a_symlink() {
    use std::os::unix::fs::symlink;
    let sb = Sandbox::new();
    let claude = sb.home.join(".claude");
    fs::write(claude.join("history.jsonl"), "{\"display\":\"x\"}\n").unwrap();
    fs::create_dir_all(claude.join("projects/repo")).unwrap();
    fs::write(claude.join("projects/repo/s.jsonl"), "{}\n").unwrap();
    fs::create_dir_all(claude.join("skills/demo")).unwrap();
    fs::write(claude.join("skills/demo/SKILL.md"), "# demo\n").unwrap();
    fs::write(sb.home.join("keep.txt"), "home\n").unwrap();
    let outside = sb.home.join("outside");
    fs::create_dir_all(outside.join("acct")).unwrap();
    fs::write(outside.join("acct/keep.txt"), "outside\n").unwrap();
    fs::write(outside.join("file.txt"), "file\n").unwrap();

    sb.ok(&["add", "work"]);
    sb.ok(&["login", "work"]);
    let dir = sb.claude_dir("work");
    symlink(&sb.home, dir.join("home")).unwrap();
    symlink(outside.join("acct"), dir.join("dir")).unwrap();
    symlink(outside.join("file.txt"), dir.join("file")).unwrap();
    symlink(sb.home.join("missing"), dir.join("broken")).unwrap();
    fs::create_dir_all(dir.join("cache/nested")).unwrap();
    symlink(&sb.home, dir.join("cache/nested/home")).unwrap();

    sb.ok(&["remove", "work"]);
    assert!(fs::symlink_metadata(&dir).is_err());

    // An account directory that is itself a symlink: only the link goes.
    let linked = sb.data().join("claude/linked");
    symlink(outside.join("acct"), &linked).unwrap();
    sb.ok(&["remove", "linked"]);
    assert!(fs::symlink_metadata(&linked).is_err());

    for bad in [
        "..",
        ".",
        "/",
        "",
        "../claude",
        "café",
        "a/../..",
        "default",
    ] {
        let out = sb.cmd(&["remove", bad]).output().unwrap();
        assert!(!out.status.success(), "remove {bad:?}");
    }

    for (path, text) in [
        (claude.join("history.jsonl"), "{\"display\":\"x\"}\n"),
        (claude.join("projects/repo/s.jsonl"), "{}\n"),
        (claude.join("skills/demo/SKILL.md"), "# demo\n"),
        (sb.home.join("keep.txt"), "home\n"),
        (outside.join("acct/keep.txt"), "outside\n"),
        (outside.join("file.txt"), "file\n"),
    ] {
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            text,
            "{}",
            path.display()
        );
    }
    assert!(sb.data().join("claude").is_dir());
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
        .env("HATS_HOME", alt.join("data"))
        .env("XDG_STATE_HOME", alt.join("xdg-state"))
        .output()
        .unwrap();
    common::assert_success(&out, &["add", "work"]);
    assert!(alt.join("data/claude/work").is_dir());
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

#[test]
fn history_a_new_account_writes_is_shared_and_survives_remove() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    let dir = sb.claude_dir("work");
    // What Claude Code writes in its first session of the new account.
    let mut history = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("history.jsonl"))
        .unwrap();
    history.write_all(b"{\"display\":\"x\"}\n").unwrap();
    for name in ["file-history", "todos", "plans"] {
        fs::create_dir_all(dir.join(name)).unwrap();
        fs::write(dir.join(name).join("entry"), name).unwrap();
    }

    sb.ok(&["remove", "work"]);

    let claude = sb.home.join(".claude");
    assert_eq!(
        fs::read_to_string(claude.join("history.jsonl")).unwrap(),
        "{\"display\":\"x\"}\n"
    );
    for name in ["file-history", "todos", "plans"] {
        assert_eq!(
            fs::read_to_string(claude.join(name).join("entry")).unwrap(),
            name
        );
    }
}

#[test]
fn help_describes_the_label_argument() {
    let sb = Sandbox::new();
    for command in ["add", "login", "use", "run", "env", "remove"] {
        let help = sb.ok(&[command, "--help"]);
        assert!(help.contains("Account label"), "{command}: {help}");
    }
}

#[test]
fn concurrent_runs_link_a_new_shared_entry_once() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    // A shared entry that appeared after `add`: every run tries to link it.
    fs::write(sb.home.join(".claude/CLAUDE.md"), "# rules\n").unwrap();
    let children: Vec<_> = (0..16)
        .map(|_| {
            sb.cmd(&["run", "work"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(
        fs::read_link(sb.claude_dir("work").join("CLAUDE.md")).unwrap(),
        sb.home.join(".claude/CLAUDE.md")
    );
}
