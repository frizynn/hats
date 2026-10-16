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

#[test]
fn another_accounts_session_does_not_move_the_default() {
    let sb = three_accounts();
    sb.repaint("side", 99.0, 0.0);
    assert_eq!(sb.current(), "work");
}
