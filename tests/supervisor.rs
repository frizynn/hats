//! `hats run` in auto mode stays as the parent of an interactive Claude Code
//! session and moves it to the next account once it passed the threshold and
//! the agent is idle. The fake `claude` below calls the real hats hooks and
//! status line the way Claude Code does, then waits for SIGTERM.

mod common;

use std::fs;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::Sandbox;

const FAKE: &str = r#"#!/bin/sh
echo "start dir=$CLAUDE_CONFIG_DIR args=$*" >> "$FAKE_LOG"
case " $* " in *" --resume "*) echo resumed >> "$FAKE_LOG"; exit 0 ;; esac
s=s-1
printf '{"session_id":"%s","source":"startup"}' $s | "$HATS" hook session-start
printf '{"session_id":"%s"}' $s | "$HATS" hook prompt
printf '{"session_id":"%s","rate_limits":{"five_hour":{"used_percentage":95,"resets_at":4102444800}}}' $s | "$HATS" statusline > /dev/null
echo "turn ended" >> "$FAKE_LOG"
printf '{"session_id":"%s","hook_event_name":"Stop"}' $s | "$HATS" hook stop
trap 'echo term >> "$FAKE_LOG"; exit 143' TERM
while :; do sleep 0.05; done
"#;

/// A pseudo-terminal: hats only supervises interactive sessions.
fn pty() -> (fs::File, fs::File) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty fills two descriptors we then own.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0);
    // SAFETY: fresh descriptors from openpty.
    unsafe { (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave)) }
}

struct Run {
    status: std::process::ExitStatus,
    elapsed: Duration,
    log: String,
}

fn two_accounts() -> Sandbox {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.signed_in("side", "org-side");
    sb.ok(&["use", "work"]);
    // The idle hooks are what make a move safe; setup installs them.
    sb.ok(&["setup"]);
    sb
}

/// `hats run -- --dangerously-skip-permissions` in a pseudo-terminal with
/// `fake` as Claude Code; `during` runs once the agent is up.
fn run_in_pty(sb: &Sandbox, fake: &str, during: impl FnOnce(u32)) -> Run {
    let bin = sb.home.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("claude"), fake).unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
    let log = sb.home.join("fake.log");

    let (mut master, slave) = pty();
    std::thread::spawn(move || {
        let mut sink = [0u8; 4096];
        while master.read(&mut sink).is_ok_and(|n| n > 0) {}
    });
    let mut child = sb
        .cmd(&["run", "--", "--dangerously-skip-permissions"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HATS", env!("CARGO_BIN_EXE_hats"))
        .env("FAKE_LOG", &log)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();

    let started = Instant::now();
    while !fs::read_to_string(&log).is_ok_and(|l| l.contains("start")) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the agent never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    during(child.id());
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            panic!(
                "hats run did not finish; log:\n{}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let elapsed = started.elapsed();
    let log = fs::read_to_string(&log).unwrap();
    Run {
        status,
        elapsed,
        log,
    }
}

#[test]
fn moves_an_idle_session_to_the_next_account_and_resumes_it() {
    let sb = two_accounts();
    let Run {
        status,
        elapsed,
        log,
    } = run_in_pty(&sb, FAKE, |_| {});
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        [
            format!(
                "start dir={} args=--dangerously-skip-permissions",
                sb.claude_dir("work").display()
            )
            .as_str(),
            "turn ended",
            "term",
            format!(
                "start dir={} args=--dangerously-skip-permissions --resume s-1",
                sb.claude_dir("side").display()
            )
            .as_str(),
            "resumed",
        ],
        "{log}"
    );
    assert!(
        status.success(),
        "hats exits with the resumed session's status"
    );
    // One second of grace before SIGTERM, then the relaunch.
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");

    let lines = sb.registry_lines();
    let moved = lines
        .iter()
        .find(|l| l["session"] == "s-1" && l["label"] == "side")
        .expect("session line for the move");
    assert_eq!(moved["reason"], "threshold");
    assert_eq!(
        sb.current(),
        "side",
        "new sessions also start on the account with room"
    );
    // The parent's run directory is gone.
    let run = sb.home.join(".local/state/hats/run");
    assert!(fs::read_dir(&run).map(|d| d.count() == 0).unwrap_or(true));
}

#[test]
fn a_prompt_during_the_grace_period_waits_for_the_next_idle_point() {
    let sb = two_accounts();
    // After the first turn the user types again at once; the move must wait
    // until that turn ends.
    let fake = FAKE.replace(
        "trap 'echo term",
        "printf '{\"session_id\":\"s-1\"}' | \"$HATS\" hook prompt\necho second turn >> \"$FAKE_LOG\"\nsleep 1.5\necho second turn ended >> \"$FAKE_LOG\"\nprintf '{\"session_id\":\"s-1\",\"hook_event_name\":\"Stop\"}' | \"$HATS\" hook stop\ntrap 'echo term",
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(
        &lines[1..5],
        ["turn ended", "second turn", "second turn ended", "term"],
        "{}",
        run.log
    );
    assert!(run.log.ends_with("resumed\n"), "{}", run.log);
}

#[test]
fn passes_the_agents_exit_status_through() {
    let sb = two_accounts();
    let fake = "#!/bin/sh\necho start >> \"$FAKE_LOG\"\nexit 7\n";
    let run = run_in_pty(&sb, fake, |_| {});
    assert_eq!(run.status.code(), Some(7));
}

#[test]
fn a_closed_terminal_ends_both() {
    let sb = two_accounts();
    let fake = "#!/bin/sh\ntrap 'echo hup >> \"$FAKE_LOG\"; exit 129' HUP\necho start >> \"$FAKE_LOG\"\nwhile :; do sleep 0.05; done\n";
    let run = run_in_pty(&sb, fake, |pid| {
        // SAFETY: signalling our own child.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
    });
    assert!(run.log.contains("hup"), "{}", run.log);
    assert_eq!(run.status.code(), Some(129));
    assert!(run.elapsed < Duration::from_secs(5));
}

#[test]
fn without_the_idle_hooks_a_session_is_never_supervised() {
    let sb = two_accounts();
    sb.ok(&["setup", "--undo"]);
    let fake = "#!/bin/sh\necho \"start parent=${HATS_SUPERVISOR:-none}\" >> \"$FAKE_LOG\"\n";
    let run = run_in_pty(&sb, fake, |_| {});
    assert_eq!(run.log.trim(), "start parent=none");
}

#[test]
fn an_agent_that_ignores_sigterm_is_killed_after_ten_seconds() {
    let sb = two_accounts();
    let fake = FAKE.replace(
        "trap 'echo term >> \"$FAKE_LOG\"; exit 143' TERM",
        "trap '' TERM",
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    assert!(run.log.ends_with("resumed\n"), "{}", run.log);
    assert!(run.elapsed >= Duration::from_secs(11), "{:?}", run.elapsed);
    assert!(run.status.success());
}
