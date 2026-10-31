//! A session cut by a login the provider refused or by a rate limit moves to
//! another account and is woken there once: `hats hook revive` is the
//! `StopFailure` hook Claude Code runs in the background with `asyncRewake`,
//! and its exit code 2 wakes the session.

mod common;

use std::fs;
use std::io::Write;
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

use common::Sandbox;

fn failure(session: &str, error: &str) -> String {
    format!(
        r#"{{"session_id":"{session}","hook_event_name":"StopFailure","error":"{error}","last_assistant_message":"Login expired · Please run /login"}}"#
    )
}

fn spawn_hook(sb: &Sandbox, args: &[&str], input: &str) -> Child {
    let mut child = sb
        .cmd(args)
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
    child
}

fn hook(sb: &Sandbox, args: &[&str], input: &str) -> Output {
    spawn_hook(sb, args, input).wait_with_output().unwrap()
}

/// The child's output once it exits within `secs`, or `None` while it runs.
fn exited(child: &mut Child, secs: u64) -> Option<i32> {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status.code().unwrap_or(-1));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn stderr(child: Child) -> String {
    String::from_utf8(child.wait_with_output().unwrap().stderr).unwrap()
}

/// The seat changed long enough ago that Claude Code reads its login again.
fn settle(sb: &Sandbox) {
    let path = sb.home.join(".local/state/hats/seat-changed");
    if let Ok(file) = fs::File::options().write(true).open(path) {
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
    }
}

fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < until, "never: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn listed(sb: &Sandbox, label: &str) -> serde_json::Value {
    let doc: serde_json::Value = serde_json::from_str(&sb.ok(&["list", "--json"])).unwrap();
    doc["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["label"] == label)
        .unwrap()
        .clone()
}

#[test]
fn the_account_in_use_leaves_once_its_login_is_refused() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    // The provider knows no token of `work`: it answers 401.
    sb.provider_says("token-org-side", 10.0, 5.0);

    sb.poll();
    assert_eq!(listed(&sb, "work")["loginRefused"], true);
    assert_eq!(sb.current(), "side");
    let last = sb.registry_lines().pop().unwrap();
    assert_eq!(last["reason"], "login");
    assert!(sb.ok(&["auto"]).contains("work's login expired"));
}

#[test]
fn an_authentication_error_marks_the_account_and_moves_once() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.signed_in("spare", "org-spare");
    sb.ok(&["use", "work"]);
    settle(&sb);
    let changes = sb.registry_lines().len();

    // Two sessions fail together: one change of account.
    let a = spawn_hook(
        &sb,
        &["hook", "stop"],
        &failure("s-1", "authentication_failed"),
    );
    let b = spawn_hook(
        &sb,
        &["hook", "stop"],
        &failure("s-2", "authentication_failed"),
    );
    assert!(a.wait_with_output().unwrap().status.success());
    assert!(b.wait_with_output().unwrap().status.success());

    assert_eq!(listed(&sb, "work")["loginRefused"], true);
    assert_ne!(sb.current(), "work");
    let lines = sb.registry_lines();
    assert_eq!(lines.len(), changes + 1, "{lines:?}");
    assert_eq!(lines.last().unwrap()["reason"], "login");
}

#[test]
fn the_cut_session_is_woken_once_on_the_new_account() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    settle(&sb);

    let mut revive = spawn_hook(&sb, &["hook", "revive"], &failure("s-1", "rate_limit"));
    until("moved to side", || sb.current() == "side");
    // A second failure of the same session while the first waits.
    let mut again = spawn_hook(&sb, &["hook", "revive"], &failure("s-1", "rate_limit"));
    assert_eq!(exited(&mut again, 5), Some(0), "one waiter per session");
    assert!(stderr(again).is_empty());
    // Claude Code still holds the login that left for a while.
    assert_eq!(exited(&mut revive, 3), None, "waits for the seat to settle");

    settle(&sb);
    assert_eq!(exited(&mut revive, 10), Some(2));
    let said = stderr(revive);
    assert!(
        said.contains("'side'") && said.contains("ontinue"),
        "{said}"
    );
}

#[test]
fn a_prompt_typed_meanwhile_or_another_kind_of_stop_wakes_nothing() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.ok(&["use", "work"]);
    settle(&sb);

    // Nowhere to go: it waits, and the user takes over.
    let mut revive = spawn_hook(&sb, &["hook", "revive"], &failure("s-1", "rate_limit"));
    assert_eq!(exited(&mut revive, 3), None);
    let typed = r#"{"session_id":"s-1","hook_event_name":"UserPromptSubmit","prompt":"go on"}"#;
    assert!(hook(&sb, &["hook", "prompt"], typed).status.success());
    assert_eq!(exited(&mut revive, 10), Some(0));
    assert!(stderr(revive).is_empty());

    // Esc runs no hook; an ordinary stop or another error is not a cut.
    let stop = r#"{"session_id":"s-2","hook_event_name":"Stop"}"#;
    let mut plain = spawn_hook(&sb, &["hook", "revive"], stop);
    assert_eq!(exited(&mut plain, 5), Some(0));
    let mut busy = spawn_hook(&sb, &["hook", "revive"], &failure("s-3", "overloaded"));
    assert_eq!(exited(&mut busy, 5), Some(0));
}

#[test]
fn with_no_account_to_move_to_it_says_so_and_wakes_when_one_comes_back() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.ok(&["use", "work"]);
    settle(&sb);

    let out = hook(&sb, &["hook", "stop"], &failure("s-1", "rate_limit"));
    let said = String::from_utf8(out.stdout).unwrap();
    let doc: serde_json::Value = serde_json::from_str(said.trim()).unwrap();
    let sequence = doc["terminalSequence"].as_str().unwrap();
    assert!(sequence.contains("no other account"), "{sequence}");
    assert!(sequence.starts_with("\u{1b}]"), "an OSC notification");
    assert_eq!(sb.current(), "work");

    let mut revive = spawn_hook(&sb, &["hook", "revive"], &failure("s-1", "rate_limit"));
    assert_eq!(exited(&mut revive, 4), None, "no loop: it waits");

    // An account comes back: it moves there and wakes the session.
    sb.signed_in("side", "org-side");
    until("moved to side", || sb.current() == "side");
    settle(&sb);
    assert_eq!(exited(&mut revive, 10), Some(2));
}

#[test]
fn a_waiter_leaves_when_claude_code_is_gone() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.ok(&["use", "work"]);
    settle(&sb);
    // Claude Code (the outer shell) runs it through a shell that does not
    // exec it; `kill -9` of Claude Code orphans that shell, not hats.
    let hats = env!("CARGO_BIN_EXE_hats");
    let sandbox = sb.cmd(&[]);
    let mut claude = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "/bin/sh -c '\"$0\" hook revive; true' \"$0\"; true",
            hats,
        ])
        .env_clear()
        .envs(sandbox.get_envs().filter_map(|(k, v)| Some((k, v?))))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    claude
        .stdin
        .take()
        .unwrap()
        .write_all(failure("s-1", "rate_limit").as_bytes())
        .unwrap();
    let file = sb.home.join(".local/state/hats/revive/s-1");
    until("waiting", || file.exists());
    let pid: i32 = fs::read_to_string(&file).unwrap().trim().parse().unwrap();

    claude.kill().unwrap();
    claude.wait().unwrap();
    until("the waiter left", || {
        // SAFETY: kill(2) with signal 0 only checks.
        unsafe { libc::kill(pid, 0) != 0 }
    });
    assert!(!file.exists());
}

#[test]
fn a_config_saved_half_way_does_not_end_the_wait() {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.ok(&["use", "work"]);
    settle(&sb);
    let mut revive = spawn_hook(&sb, &["hook", "revive"], &failure("s-1", "rate_limit"));
    assert_eq!(exited(&mut revive, 2), None);

    let config = sb.home.join(".config/hats/config.toml");
    let saved = fs::read_to_string(&config).unwrap_or_default();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "[claude\n").unwrap();
    assert_eq!(exited(&mut revive, 3), None, "still waiting");
    fs::write(&config, saved).unwrap();

    sb.signed_in("side", "org-side");
    until("moved to side", || sb.current() == "side");
    settle(&sb);
    assert_eq!(exited(&mut revive, 10), Some(2));
}
