//! Auto mode moves the default account off one that passed the threshold or
//! hit its limit, using only the limits the harness already reported.

mod common;

use std::io::Write;
use std::process::Stdio;

use common::Sandbox;

fn three_accounts() -> Sandbox {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.signed_in("spare", "org-spare");
    sb.ok(&["use", "work"]);
    sb
}

fn reasons(sb: &Sandbox) -> Vec<(String, String)> {
    sb.registry_lines()
        .iter()
        .filter(|l| l.get("session").is_none())
        .map(|l| {
            (
                l["label"].as_str().unwrap().to_owned(),
                l["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn passing_the_threshold_moves_new_sessions_to_the_emptiest_account() {
    let sb = three_accounts();
    sb.repaint("side", 40.0, 10.0);
    sb.repaint("spare", 5.0, 20.0);
    sb.repaint("work", 89.0, 50.0);
    assert_eq!(sb.current(), "work", "under the threshold nothing moves");

    let out = sb.repaint("work", 91.0, 50.0);
    assert!(out.status.success());
    assert_eq!(sb.current(), "spare");
    assert_eq!(
        reasons(&sb).last().unwrap(),
        &("spare".to_owned(), "threshold".to_owned())
    );
}

#[test]
fn reaching_the_limit_is_recorded_as_limit() {
    let sb = three_accounts();
    sb.repaint("work", 30.0, 100.0);
    assert_ne!(sb.current(), "work");
    assert_eq!(reasons(&sb).last().unwrap().1, "limit");
}

#[test]
fn next_strategy_follows_the_configured_order() {
    let sb = three_accounts();
    sb.ok(&["config", "set", "claude.strategy", "next"]);
    sb.ok(&["config", "set", "claude.order", "work,side,spare"]);
    sb.repaint("side", 85.0, 0.0); // over the margin: skipped
    sb.repaint("work", 95.0, 0.0);
    assert_eq!(sb.current(), "spare");
}

#[test]
fn manual_mode_never_moves() {
    let sb = three_accounts();
    sb.ok(&["config", "set", "claude.mode", "manual"]);
    sb.repaint("work", 100.0, 100.0);
    assert_eq!(sb.current(), "work");
}

#[test]
fn without_an_account_with_room_it_stays() {
    let sb = three_accounts();
    sb.repaint("side", 95.0, 0.0);
    sb.repaint("spare", 0.0, 88.0);
    sb.repaint("work", 99.0, 0.0);
    assert_eq!(sb.current(), "work");
    assert_eq!(reasons(&sb).len(), 1, "only the manual `use`");
}

#[test]
fn a_window_that_already_reset_counts_as_empty() {
    let sb = three_accounts();
    // `side` was at 99 % in a window that reset in 2000: it has room again.
    use std::io::Write;
    let stale = r#"{"session_id":"x","rate_limits":{"five_hour":{"used_percentage":99,"resets_at":946684800}}}"#;
    let mut child = sb
        .cmd(&["statusline"])
        .env("CLAUDE_CONFIG_DIR", sb.claude_dir("side"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stale.as_bytes())
        .unwrap();
    child.wait().unwrap();
    sb.repaint("spare", 70.0, 70.0);
    sb.repaint("work", 95.0, 0.0);
    assert_eq!(sb.current(), "side");
}

#[test]
fn run_starts_new_sessions_on_an_account_with_room() {
    let sb = three_accounts();
    sb.ok(&["config", "set", "claude.mode", "manual"]);
    sb.repaint("work", 97.0, 0.0);
    sb.repaint("side", 10.0, 0.0);
    sb.repaint("spare", 50.0, 0.0);
    sb.ok(&["config", "set", "claude.mode", "auto"]);

    let out = sb.ok(&["run"]);
    assert!(
        out.contains(&format!(
            "CLAUDE_CONFIG_DIR={}",
            sb.claude_dir("side").display()
        )),
        "{out}"
    );
    // An explicit label is the user's choice and is respected.
    let out = sb.ok(&["run", "work"]);
    assert!(
        out.contains(&format!(
            "CLAUDE_CONFIG_DIR={}",
            sb.claude_dir("work").display()
        )),
        "{out}"
    );
}

#[test]
fn a_rate_limit_error_moves_the_default_without_limit_data() {
    let sb = three_accounts();
    let mut child = sb
        .cmd(&["hook", "stop"])
        .env("CLAUDE_CONFIG_DIR", sb.claude_dir("work"))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"s-1","hook_event_name":"StopFailure","error":"rate_limit"}"#)
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_ne!(sb.current(), "work");
    assert_eq!(reasons(&sb).last().unwrap().1, "limit");
}

/// Ends a turn of `label`'s session with a rate limit error, as Claude Code's
/// `StopFailure` hook does.
fn rate_limited(sb: &Sandbox, label: &str) -> std::process::Child {
    let mut child = sb
        .cmd(&["hook", "stop"])
        .env("CLAUDE_CONFIG_DIR", sb.claude_dir(label))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let input = format!(
        r#"{{"session_id":"s-{label}","hook_event_name":"StopFailure","error":"rate_limit"}}"#
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child
}

#[test]
fn an_account_never_seen_comes_after_one_with_room() {
    let sb = three_accounts();
    // `side` was never seen: it may be the spent one.
    sb.repaint("spare", 30.0, 30.0);
    assert!(rate_limited(&sb, "work").wait().unwrap().success());
    assert_eq!(sb.current(), "spare");
}

#[test]
fn another_accounts_session_does_not_move_the_default() {
    let sb = three_accounts();
    sb.repaint("side", 99.0, 0.0);
    assert_eq!(sb.current(), "work");
}

/// A Codex rollout as Codex writes it: a session header, then events; the
/// last `token_count` carries the account's limits.
fn rollout(sb: &Sandbox, label: &str, account_id: &str, used: f64) {
    let dir = sb
        .data()
        .join("codex")
        .join(label)
        .join("sessions/2026/01/02");
    std::fs::create_dir_all(&dir).unwrap();
    let header = format!(
        r#"{{"type":"session_meta","payload":{{"id":"x","creator_account_id":"{account_id}"}}}}"#
    );
    let old = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":1.0,"window_minutes":300,"resets_at":4102444800}}}}"#;
    let new = format!(
        r#"{{"type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used},"window_minutes":300,"resets_at":4102444800}},"secondary":null,"plan_type":"pro"}}}}}}"#
    );
    std::fs::write(
        dir.join("rollout-2026-01-02T10-00-00-x.jsonl"),
        format!("{header}\n{old}\n{new}\n"),
    )
    .unwrap();
}

#[test]
fn codex_new_sessions_move_using_rollout_limits() {
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.home.join(".codex")).unwrap();
    sb.ok(&["--codex", "add", "main"]);
    sb.ok(&["--codex", "add", "alt"]);
    rollout(&sb, "main", "acct-main", 96.0);
    rollout(&sb, "alt", "acct-alt", 30.0);
    sb.ok(&["--codex", "use", "main"]);

    let list = sb.ok(&["list", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&list).unwrap();
    let main = doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == "main")
        .unwrap();
    assert_eq!(main["usedPercent"], 96.0);
    assert_eq!(main["limits"]["windows"][0]["name"], "five_hour");

    let out = sb.ok(&["--codex", "run"]);
    let alt = sb.data().join("codex/alt");
    assert!(
        out.contains(&format!("CODEX_HOME={}", alt.display())),
        "{out}"
    );
    let last = sb.registry_lines().pop().unwrap();
    assert_eq!(
        (
            last["harness"].as_str(),
            last["label"].as_str(),
            last["reason"].as_str()
        ),
        (Some("codex"), Some("alt"), Some("threshold"))
    );
}
