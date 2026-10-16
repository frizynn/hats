//! The status line wrapper keeps the last plan limits Claude Code showed for
//! each account in `limits.json`, and hands the same input to the user's own
//! status line command.

mod common;

use std::fs;
use std::io::Write;
use std::process::{Output, Stdio};

use common::{Sandbox, mode};

const INPUT: &str = r#"{"session_id":"s-1","model":{"display_name":"Demo"},"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1738425600},"seven_day":{"used_percentage":41.2,"resets_at":1738857600}}}"#;

fn statusline(sb: &Sandbox, account: Option<&str>, args: &[&str], input: &str) -> Output {
    let mut argv = vec!["statusline"];
    argv.extend_from_slice(args);
    let mut cmd = sb.cmd(&argv);
    if let Some(label) = account {
        cmd.env("CLAUDE_CONFIG_DIR", sb.data().join("claude").join(label));
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

fn limits_path(sb: &Sandbox) -> std::path::PathBuf {
    sb.home.join(".local/state/hats/limits.json")
}

fn logged_in(sb: &Sandbox, label: &str) {
    sb.ok(&["add", label]);
    sb.ok(&["login", label]);
}

#[test]
fn records_limits_per_account_in_the_agreed_format() {
    let sb = Sandbox::new();
    logged_in(&sb, "work");

    let out = statusline(&sb, Some("work"), &[], INPUT);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "work 5h 24% 7d 41%"
    );

    let text = fs::read_to_string(limits_path(&sb)).unwrap();
    let observed = &text[text.find("\"observedAt\":").unwrap() + 13..];
    let observed = &observed[..observed.find(',').unwrap()];
    assert!(observed.parse::<u64>().unwrap() > 1_700_000_000_000);
    assert_eq!(
        text,
        format!(
            r#"{{"v":1,"accounts":[{{"harness":"claude","account":"9ae25bfbd05c9e76","label":"work","observedAt":{observed},"windows":[{{"name":"five_hour","usedPercent":23.5,"resetsAt":1738425600}},{{"name":"seven_day","usedPercent":41.2,"resetsAt":1738857600}}]}}]}}"#
        )
    );
    assert_eq!(mode(&limits_path(&sb)), 0o600);
}

#[test]
fn keeps_other_accounts_and_replaces_the_same_one() {
    let sb = Sandbox::new();
    logged_in(&sb, "work");
    statusline(&sb, Some("work"), &[], INPUT);
    // A second account: the fake login gives it another organization.
    sb.ok(&["add", "personal"]);
    let out = sb
        .cmd(&["login", "personal"])
        .env("FAKE_ORG_UUID", "00000000-0000-4000-8000-0000000000bb")
        .output()
        .unwrap();
    common::assert_success(&out, &["login", "personal"]);
    statusline(&sb, Some("personal"), &[], INPUT);
    statusline(&sb, Some("work"), &[], &INPUT.replace("23.5", "80"));

    let limits: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(limits_path(&sb)).unwrap()).unwrap();
    let accounts = limits["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    let work = accounts.iter().find(|a| a["label"] == "work").unwrap();
    assert_eq!(work["windows"][0]["usedPercent"], 80.0);
    let personal = accounts.iter().find(|a| a["label"] == "personal").unwrap();
    assert_eq!(personal["windows"][0]["usedPercent"], 23.5);
}

#[test]
fn chains_the_users_command_with_the_same_input() {
    let sb = Sandbox::new();
    logged_in(&sb, "work");
    let script = sb.home.join("mine.sh");
    fs::write(
        &script,
        "#!/bin/sh\nprintf 'mine:'; wc -c | tr -d ' '\nexit 3\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let out = statusline(&sb, Some("work"), &["--", script.to_str().unwrap()], INPUT);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("mine:{}", INPUT.len())
    );
    assert!(limits_path(&sb).exists());
}

#[test]
fn without_limits_or_login_it_only_shows_the_account() {
    let sb = Sandbox::new();
    sb.ok(&["add", "work"]);
    let out = statusline(&sb, Some("work"), &[], INPUT);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "work 5h 24% 7d 41%"
    );
    assert!(!limits_path(&sb).exists(), "no id before login");

    sb.ok(&["login", "work"]);
    let out = statusline(&sb, Some("work"), &[], r#"{"session_id":"s-1"}"#);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "work");
    assert!(!limits_path(&sb).exists());

    let out = statusline(&sb, None, &[], "garbage");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "default");
}

#[test]
fn unchanged_limits_are_not_rewritten_on_every_repaint() {
    let sb = Sandbox::new();
    logged_in(&sb, "work");
    statusline(&sb, Some("work"), &[], INPUT);
    let first = fs::read_to_string(limits_path(&sb)).unwrap();
    statusline(&sb, Some("work"), &[], INPUT);
    assert_eq!(fs::read_to_string(limits_path(&sb)).unwrap(), first);

    statusline(&sb, Some("work"), &[], &INPUT.replace("41.2", "42"));
    assert_ne!(fs::read_to_string(limits_path(&sb)).unwrap(), first);
}

#[test]
fn concurrent_repaints_of_every_account_lose_nothing() {
    let sb = Sandbox::new();
    let labels: Vec<String> = (0..6).map(|i| format!("a{i}")).collect();
    for label in &labels {
        sb.signed_in(label, &format!("org-{label}"));
    }
    sb.ok(&["use", "a0"]);
    // a0 is past the threshold, so its repaints also move the default.
    let mut children = Vec::new();
    for round in 0..4 {
        for (i, label) in labels.iter().enumerate() {
            let used = if i == 0 { 95 } else { 10 + i * 5 + round };
            let input = format!(
                r#"{{"session_id":"s-{label}","rate_limits":{{"five_hour":{{"used_percentage":{used},"resets_at":4102444800}}}}}}"#
            );
            let mut child = sb
                .cmd(&["statusline"])
                .env("CLAUDE_CONFIG_DIR", sb.claude_dir(label))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            children.push(child);
        }
    }
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success() && out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let doc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(limits_path(&sb)).unwrap()).unwrap();
    let mut seen: Vec<&str> = doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["label"].as_str().unwrap())
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, labels);
    assert_eq!(mode(&limits_path(&sb)), 0o600);
    // Every registry line is whole, and the default left a0 for an account with room.
    let lines = sb.registry_lines();
    assert!(lines.iter().any(|l| l["reason"] == "threshold"));
    assert_ne!(sb.current(), "a0");
    let leftovers: Vec<_> = fs::read_dir(sb.home.join(".local/state/hats"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
