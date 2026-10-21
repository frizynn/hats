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

    // Every session runs in Claude Code's own directory, with the login of
    // the account that has room.
    let out = sb.ok(&["run"]);
    assert!(out.contains("CLAUDE_CONFIG_DIR=<unset>"), "{out}");
    assert_eq!(sb.current(), "side");
    assert_eq!(sb.seat_token(), "token-org-side");
    // An explicit label is the user's choice and is respected.
    let out = sb.ok(&["run", "work"]);
    assert!(out.contains("CLAUDE_CONFIG_DIR=<unset>"), "{out}");
    assert_eq!(sb.current(), "work");
    assert_eq!(sb.seat_token(), "token-org-work");
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
    let mut child = waiting_hook(sb, label);
    release(&mut child, label);
    child
}

/// A `StopFailure` hook started but still waiting for its input.
fn waiting_hook(sb: &Sandbox, label: &str) -> std::process::Child {
    sb.cmd(&["hook", "stop"])
        .env("CLAUDE_CONFIG_DIR", sb.claude_dir(label))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap()
}

fn release(child: &mut std::process::Child, label: &str) {
    let input = format!(
        r#"{{"session_id":"s-{label}","hook_event_name":"StopFailure","error":"rate_limit"}}"#
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).unwrap();
}

#[test]
fn an_account_never_seen_comes_after_one_with_room() {
    let sb = three_accounts();
    // `side` was never seen: it may be the spent one.
    sb.repaint("spare", 30.0, 30.0);
    assert!(rate_limited(&sb, "work").wait().unwrap().success());
    assert_eq!(sb.current(), "spare");
}

fn listed(sb: &Sandbox, label: &str) -> serde_json::Value {
    let doc: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["harness"] == "claude" && a["label"] == label)
        .unwrap()
        .clone()
}

#[test]
fn a_rate_limit_counts_as_full_until_the_status_line_says_otherwise() {
    let sb = three_accounts();
    sb.repaint("side", 10.0, 10.0);
    sb.repaint("spare", 40.0, 40.0);
    // `side` hits its limit while `work` is the default: nothing moves, but
    // `side` is known to be full although its last numbers said 10 %.
    assert!(rate_limited(&sb, "side").wait().unwrap().success());
    assert_eq!(sb.current(), "work");
    let side = listed(&sb, "side");
    assert_eq!(side["usedPercent"], 100.0, "{side}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let until = side["limits"]["limitedUntil"]
        .as_i64()
        .expect("limitedUntil");
    assert!((until - (now + 5 * 3600)).abs() < 60, "{side}");

    sb.repaint("work", 95.0, 0.0);
    assert_eq!(sb.current(), "spare", "side is full");

    // New numbers from `side`'s status line replace what was learned.
    sb.ok(&["use", "work"]);
    sb.repaint("side", 12.0, 10.0);
    assert_eq!(listed(&sb, "side")["usedPercent"], 12.0);
    sb.repaint("work", 96.0, 0.0);
    assert_eq!(sb.current(), "side");
}

#[test]
fn hooks_failing_at_the_same_moment_move_the_default_once() {
    let sb = three_accounts();
    // As large as a real `.claude.json`, so deciding takes as long as it does.
    for label in ["work", "side", "spare"] {
        let path = sb.claude_dir(label).join(".claude.json");
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["projects"] = (0..700)
            .map(|i| (format!("/home/user/src/p{i:04}"), serde_json::json!({"history": [], "lastCost": 1.5, "allowedTools": ["Bash", "Read", "Edit"]})))
            .collect::<serde_json::Map<_, _>>()
            .into();
        std::fs::write(&path, config.to_string()).unwrap();
    }
    sb.repaint("side", 10.0, 10.0);
    sb.repaint("spare", 20.0, 20.0);
    let rounds = 10;
    for _ in 0..rounds {
        sb.ok(&["use", "work"]);
        // Sixteen sessions of `work` hit the limit together: every hook reads
        // its input at the same moment.
        let mut hooks: Vec<_> = (0..16).map(|_| waiting_hook(&sb, "work")).collect();
        for hook in &mut hooks {
            release(hook, "work");
        }
        for mut hook in hooks {
            assert!(hook.wait().unwrap().success());
        }
    }
    let moves = reasons(&sb)
        .into_iter()
        .filter(|(_, reason)| reason == "limit")
        .count();
    assert_eq!(moves, rounds, "one move per round");
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

#[test]
fn auto_says_what_it_does_and_turns_it_on_and_off() {
    let sb = three_accounts();
    // Wide, so each sentence stays on one line.
    let auto = |args: &[&str]| {
        let out = sb.cmd(args).env("COLUMNS", "1000").output().unwrap();
        common::assert_success(&out, args);
        String::from_utf8(out.stdout).unwrap()
    };
    let out = auto(&["auto"]);
    for line in [
        "Claude Code: automatic switching is on.",
        "When the account in use reaches 90 % of its 5-hour or weekly limit, Claude Code switches to the account with the most room, the sessions already open too.",
        "In use now: 'work'.",
        ": default → work: you chose it.",
        "Not active yet: run `hats setup` to install the hooks and the status line that switch.",
    ] {
        assert!(out.contains(line), "{line:?} in\n{out}");
    }

    // Once setup installed the hooks and the status line, nothing else is needed.
    std::fs::write(
        sb.home.join(".claude/settings.json"),
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"'/bin/hats' hook stop"}]}]},"statusLine":{"type":"command","command":"'/bin/hats' statusline"}}"#,
    )
    .unwrap();
    sb.repaint("spare", 5.0, 5.0);
    sb.repaint("work", 95.0, 0.0);
    let out = auto(&["auto"]);
    assert!(
        out.contains(": work → spare: work passed the threshold."),
        "{out}"
    );
    assert!(out.contains("Nothing needs to stay open"), "{out}");
    assert!(!out.contains("hats setup"), "{out}");

    let out = auto(&["auto", "off"]);
    assert!(
        out.starts_with("Claude Code: automatic switching is off."),
        "{out}"
    );
    assert!(out.contains("`hats auto on` turns it on."), "{out}");
    assert_eq!(sb.ok(&["config", "get", "claude.mode"]).trim(), "manual");
    assert!(auto(&["auto", "on"]).starts_with("Claude Code: automatic switching is on."));
    assert_eq!(sb.ok(&["config", "get", "claude.mode"]).trim(), "auto");

    // Codex has its own switch.
    let out = auto(&["--codex", "auto", "off"]);
    assert!(
        out.starts_with("Codex: automatic switching is off."),
        "{out}"
    );
    assert!(out.contains("No change of account yet."), "{out}");
    assert_eq!(sb.ok(&["config", "get", "codex.mode"]).trim(), "manual");
    assert_eq!(sb.ok(&["config", "get", "claude.mode"]).trim(), "auto");
}

#[test]
fn a_learned_limit_that_expired_is_not_data() {
    let sb = three_accounts();
    sb.repaint("spare", 50.0, 50.0);
    // `side` hit its limit five hours ago and has reported nothing since.
    assert!(rate_limited(&sb, "side").wait().unwrap().success());
    let path = sb.home.join(".local/state/hats/limits.json");
    let mut limits: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for entry in limits["accounts"].as_array_mut().unwrap() {
        if let Some(until) = entry.get("limitedUntil").and_then(|u| u.as_i64()) {
            entry["limitedUntil"] = (until - 5 * 3600 - 1).into();
        }
    }
    std::fs::write(&path, limits.to_string()).unwrap();
    assert_eq!(listed(&sb, "side")["usedPercent"], serde_json::Value::Null);
    // `spare` has numbers and room: it goes first.
    sb.repaint("work", 95.0, 0.0);
    assert_eq!(sb.current(), "spare");
}

#[test]
fn an_account_turned_off_is_never_switched_to_but_can_be_chosen() {
    let sb = three_accounts();
    sb.repaint("side", 40.0, 10.0);
    sb.repaint("spare", 5.0, 20.0);
    assert!(sb.ok(&["disable", "spare"]).contains("'spare' is disabled"));
    let enabled = |label: &str| listed(&sb, label)["enabled"].clone();
    assert_eq!(enabled("spare"), false);
    assert_eq!(enabled("side"), true);

    // The emptiest account is off: the threshold moves to the next one.
    sb.repaint("work", 91.0, 50.0);
    assert_eq!(sb.current(), "side");

    // Every other account off: a rate limit does not move it, and hats
    // auto says why.
    sb.ok(&["use", "work"]);
    let out = sb.ok(&["disable", "side"]);
    assert!(out.contains("nowhere to move"), "{out}");
    rate_limited(&sb, "work").wait().unwrap();
    assert_eq!(sb.current(), "work");
    let auto = sb.ok(&["auto"]);
    assert!(auto.contains("every other account is disabled"), "{auto}");

    // Chosen by hand it is used, with a warning.
    let out = sb.cmd(&["use", "spare"]).output().unwrap();
    common::assert_success(&out, &["use", "spare"]);
    assert_eq!(sb.current(), "spare");
    let warning = String::from_utf8_lossy(&out.stderr);
    assert!(warning.contains("'spare' is disabled"), "{warning}");

    sb.ok(&["enable", "spare"]);
    sb.ok(&["enable", "side"]);
    assert_eq!(enabled("spare"), true);
    let config = std::fs::read_to_string(sb.home.join(".config/hats/config.toml")).unwrap();
    assert!(config.contains("disabled = []"), "{config}");
    assert!(
        sb.cmd(&["disable", "nobody"])
            .output()
            .unwrap()
            .status
            .code()
            != Some(0)
    );
}

#[test]
fn disabling_the_account_in_use_moves_to_the_one_with_most_room() {
    let sb = three_accounts();
    sb.repaint("work", 20.0, 10.0);
    sb.repaint("side", 60.0, 10.0);
    sb.repaint("spare", 5.0, 20.0);
    let out = sb.ok(&["disable", "work"]);
    assert!(out.contains("Moved from 'work' to 'spare'"), "{out}");
    assert_eq!(sb.current(), "spare");
    assert_eq!(sb.seat_token(), "token-org-spare");
    let reasons = reasons(&sb);
    assert_eq!(
        reasons.last().unwrap(),
        &("spare".to_owned(), "disabled".to_owned())
    );

    // Chosen again by hand, it stays: only disabling it moves away.
    sb.ok(&["use", "work"]);
    sb.repaint("work", 21.0, 10.0);
    assert_eq!(sb.current(), "work");
}

#[test]
fn disabling_the_account_in_use_in_manual_mode_stays() {
    let sb = three_accounts();
    sb.repaint("spare", 5.0, 20.0);
    sb.ok(&["auto", "off"]);
    let out = sb.ok(&["disable", "work"]);
    assert!(!out.contains("Moved"), "{out}");
    assert_eq!(sb.current(), "work");
}
