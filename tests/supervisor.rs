//! `hats run` in auto mode stays as the parent of an interactive Claude Code
//! session and moves it to the next account once it passed the threshold and
//! the agent is idle. The fake `claude` below calls the real hats hooks and
//! status line the way Claude Code does, then waits for SIGTERM.

mod common;

use std::fs;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
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
    /// What the agent and hats wrote to the terminal.
    output: String,
    /// The terminal the agent ran in, to read its settings after the run.
    tty: fs::File,
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

/// Puts `fake` on PATH as `claude`, unless a session may be running it.
fn install_fake(sb: &Sandbox, fake: &str) -> std::path::PathBuf {
    let bin = sb.home.join("fake-bin");
    if fs::read_to_string(bin.join("claude")).is_ok_and(|old| old == fake) {
        return bin;
    }
    fs::create_dir_all(&bin).unwrap();
    common::write_program(&bin.join("claude"), fake);
    bin
}

/// `hats run -- --dangerously-skip-permissions` in a pseudo-terminal with
/// `fake` as Claude Code; `during` runs once the agent is up.
fn run_in_pty(sb: &Sandbox, fake: &str, during: impl FnOnce(u32)) -> Run {
    let bin = install_fake(sb, fake);
    let log = sb.home.join("fake.log");

    let (mut master, slave) = pty();
    let tty = slave.try_clone().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let written = Arc::clone(&output);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = master.read(&mut buf) {
            written.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let mut child = sb
        .cmd(&["run", "--", "--dangerously-skip-permissions"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HATS", env!("CARGO_BIN_EXE_hats"))
        .env("FAKE_LOG", &log)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave))
        // Its own process group, so a timeout can stop the agent with it.
        .process_group(0)
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
            // SAFETY: the process group created for this child.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            panic!(
                "hats run did not finish; log:\n{}",
                fs::read_to_string(&log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let elapsed = started.elapsed();
    let log = fs::read_to_string(&log).unwrap();
    // The reader thread may not have drained the terminal yet.
    std::thread::sleep(Duration::from_millis(150));
    let output = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    Run {
        status,
        elapsed,
        log,
        output,
        tty,
    }
}

#[test]
fn moves_an_idle_session_to_the_next_account_and_resumes_it() {
    let sb = two_accounts();
    let Run {
        status,
        elapsed,
        log,
        ..
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

fn local_flags(tty: &fs::File) -> libc::tcflag_t {
    use std::os::fd::AsRawFd;
    let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr fills the struct on success, checked below.
    assert_eq!(
        unsafe { libc::tcgetattr(tty.as_raw_fd(), t.as_mut_ptr()) },
        0
    );
    // SAFETY: initialized by tcgetattr.
    unsafe { t.assume_init() }.c_lflag
}

#[test]
fn an_agent_killed_outright_leaves_a_usable_terminal() {
    let sb = two_accounts();
    // Raw mode without echo, as an interactive agent sets it, then a kill it
    // cannot handle (the OOM killer, `kill -9`).
    let fake = "#!/bin/sh\nstty raw -echo\necho start >> \"$FAKE_LOG\"\nkill -KILL $$\n";
    let run = run_in_pty(&sb, fake, |_| {});
    assert_eq!(run.status.code(), Some(128 + libc::SIGKILL));
    let flags = local_flags(&run.tty);
    assert!(
        flags & libc::ECHO != 0 && flags & libc::ICANON != 0,
        "echo and line editing are back: {flags:#x}"
    );
}

#[test]
fn a_stop_request_during_a_move_ends_instead_of_relaunching() {
    let sb = two_accounts();
    // The terminal closes while the agent is shutting down for the move.
    let fake = FAKE.replace(
        "trap 'echo term >> \"$FAKE_LOG\"; exit 143' TERM",
        "trap 'echo term >> \"$FAKE_LOG\"; kill -HUP $HATS_SUPERVISOR; sleep 0.3; exit 143' TERM",
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    assert!(run.log.contains("term"), "{}", run.log);
    assert!(!run.log.contains("--resume"), "relaunched: {}", run.log);
    assert!(!run.status.success());
}

#[test]
fn the_agent_ends_when_its_parent_is_killed() {
    let sb = two_accounts();
    let fake = "#!/bin/sh\ntrap '' INT\ntrap 'echo hup >> \"$FAKE_LOG\"; exit 129' HUP\necho start >> \"$FAKE_LOG\"\nwhile :; do sleep 0.05; done\n";
    let log = sb.home.join("fake.log");
    let group = std::cell::Cell::new(0);
    // Whatever happens, nothing of this test outlives it.
    let _cleanup = KillGroupOnDrop(&group);
    let run = run_in_pty(&sb, fake, |pid| {
        group.set(pid as libc::pid_t);
        // SAFETY: signals to our own child's process group, then to the child.
        unsafe {
            // Ctrl+C reaches the whole group first: it must not disarm anything.
            libc::kill(-(pid as libc::pid_t), libc::SIGINT);
            std::thread::sleep(Duration::from_millis(200));
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    });
    assert_eq!(run.status.signal(), Some(libc::SIGKILL));
    let started = Instant::now();
    while !fs::read_to_string(&log).unwrap().contains("hup") {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the agent outlived its parent: {}",
            fs::read_to_string(&log).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct KillGroupOnDrop<'a>(&'a std::cell::Cell<libc::pid_t>);

impl Drop for KillGroupOnDrop<'_> {
    fn drop(&mut self) {
        if self.0.get() > 0 {
            // SAFETY: the process group created for the test's child.
            unsafe { libc::kill(-self.0.get(), libc::SIGKILL) };
        }
    }
}

#[test]
fn two_sessions_on_the_same_account_move_independently() {
    let sb = two_accounts();
    install_fake(&sb, FAKE);
    let runs: Vec<Run> = std::thread::scope(|s| {
        let a = s.spawn(|| run_in_pty(&sb, FAKE, |_| {}));
        let b = s.spawn(|| run_in_pty(&sb, FAKE, |_| {}));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    // Both write to the same log.
    let log = fs::read_to_string(sb.home.join("fake.log")).unwrap();
    for run in &runs {
        assert!(run.status.success(), "{log}");
    }
    assert_eq!(log.matches("term").count(), 2, "{log}");
    assert_eq!(log.matches("resumed").count(), 2, "{log}");
    let run = sb.home.join(".local/state/hats/run");
    assert!(fs::read_dir(&run).map(|d| d.count() == 0).unwrap_or(true));
}

#[test]
fn a_threshold_seen_long_after_the_turn_ended_waits_for_the_next_turn() {
    let sb = two_accounts();
    // The turn ends below the threshold; much later, while the user may be
    // typing, a repaint (a permission mode change, the prompt cache expiring)
    // reports the threshold passed.
    let fake = r#"#!/bin/sh
echo "start dir=$CLAUDE_CONFIG_DIR args=$*" >> "$FAKE_LOG"
case " $* " in *" --resume "*) echo resumed >> "$FAKE_LOG"; exit 0 ;; esac
trap 'echo term >> "$FAKE_LOG"; exit 143' TERM
s=s-1
printf '{"session_id":"%s","source":"startup"}' $s | "$HATS" hook session-start
printf '{"session_id":"%s"}' $s | "$HATS" hook prompt
echo "turn ended" >> "$FAKE_LOG"
printf '{"session_id":"%s","hook_event_name":"Stop"}' $s | "$HATS" hook stop
sleep 4
printf '{"session_id":"%s","rate_limits":{"five_hour":{"used_percentage":95,"resets_at":4102444800}}}' $s | "$HATS" statusline > /dev/null
echo "late repaint" >> "$FAKE_LOG"
sleep 2
echo "second turn" >> "$FAKE_LOG"
printf '{"session_id":"%s"}' $s | "$HATS" hook prompt
printf '{"session_id":"%s","hook_event_name":"Stop"}' $s | "$HATS" hook stop
while :; do sleep 0.05; done
"#;
    let run = run_in_pty(&sb, fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(
        &lines[1..5],
        ["turn ended", "late repaint", "second turn", "term"],
        "{}",
        run.log
    );
    assert!(run.log.ends_with("resumed\n"), "{}", run.log);
}

/// One turn that ends, then an idle agent until SIGTERM. `$T` is the
/// session's transcript; `# later` marks where a test adds what happens after.
const IDLE: &str = r#"#!/bin/sh
echo "start args=$* parent=${HATS_SUPERVISOR:-none}" >> "$FAKE_LOG"
trap 'echo term >> "$FAKE_LOG"; exit 143' TERM
s=s-1
T="$HOME/$s.jsonl"
: > "$T"
stop() { printf '{"session_id":"%s","transcript_path":"%s","hook_event_name":"Stop"}' $s "$T" | "$HATS" hook stop; }
printf '{"session_id":"%s","source":"startup"}' $s | "$HATS" hook session-start
printf '{"session_id":"%s"}' $s | "$HATS" hook prompt
stop
echo "turn ended" >> "$FAKE_LOG"
# later
while :; do sleep 0.05; done
"#;

fn later(steps: &str) -> String {
    IDLE.replace("# later", steps)
}

fn close_idle(sb: &Sandbox, value: &str) {
    sb.ok(&["config", "set", "claude.close_idle", value]);
}

#[test]
fn closes_a_session_left_idle_and_says_how_to_resume_it() {
    let sb = two_accounts();
    close_idle(&sb, "2s");
    let run = run_in_pty(&sb, IDLE, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(&lines[1..], ["turn ended", "term"], "{}", run.log);
    assert!(
        run.output.contains(
            "hats: closed this session after 2s without use. Resume it with: claude --resume s-1"
        ),
        "{:?}",
        run.output
    );
    assert!(run.status.success(), "the shell gets its prompt back");
    assert!(run.elapsed >= Duration::from_secs(2), "{:?}", run.elapsed);
    assert!(run.elapsed < Duration::from_secs(5), "{:?}", run.elapsed);
    assert_eq!(sb.current(), "work", "closing is not a move");
    let run = sb.home.join(".local/state/hats/run");
    assert!(fs::read_dir(&run).map(|d| d.count() == 0).unwrap_or(true));
}

#[test]
fn never_closes_during_a_turn() {
    let sb = two_accounts();
    close_idle(&sb, "1s");
    // A turn, or a wait for approval inside one, longer than the limit:
    // after UserPromptSubmit and before Stop the agent is busy.
    let fake = later(
        r#"printf '{"session_id":"%s"}' $s | "$HATS" hook prompt
echo "long turn" >> "$FAKE_LOG"
sleep 3
echo "long turn ended" >> "$FAKE_LOG"
stop"#,
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(
        &lines[1..],
        ["turn ended", "long turn", "long turn ended", "term"],
        "{}",
        run.log
    );
    assert!(run.elapsed >= Duration::from_secs(4), "{:?}", run.elapsed);
}

#[test]
fn a_prompt_restarts_the_clock() {
    let sb = two_accounts();
    close_idle(&sb, "2s");
    // Without the second turn the session would close at 2 s; the turn ends
    // at about 2.5 s, so it closes at about 4.5 s.
    let fake = later(
        r#"sleep 1.5
printf '{"session_id":"%s"}' $s | "$HATS" hook prompt
sleep 1
stop
echo "second turn ended" >> "$FAKE_LOG""#,
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(
        &lines[1..],
        ["turn ended", "second turn ended", "term"],
        "{}",
        run.log
    );
    assert!(
        run.elapsed >= Duration::from_millis(4300),
        "{:?}",
        run.elapsed
    );
}

#[test]
fn a_transcript_write_restarts_the_clock() {
    let sb = two_accounts();
    close_idle(&sb, "2s");
    // A background task wakes the agent without UserPromptSubmit; the
    // transcript still grows. Without that write it would close at 2 s.
    let fake = later(
        r#"sleep 1.5
echo '{"type":"user"}' >> "$T"
sleep 1.5
echo "after the write" >> "$FAKE_LOG""#,
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(
        &lines[1..],
        ["turn ended", "after the write", "term"],
        "{}",
        run.log
    );
    assert!(
        run.elapsed >= Duration::from_millis(3300),
        "{:?}",
        run.elapsed
    );
}

#[test]
fn off_never_closes() {
    let sb = two_accounts();
    for off in ["0", "off"] {
        close_idle(&sb, off);
        assert_eq!(sb.ok(&["config", "get", "claude.close_idle"]).trim(), "off");
    }
    let fake = later(
        r#"sleep 2.5
echo "still open" >> "$FAKE_LOG"
exit 0"#,
    );
    // Short only for the test: with it off, nothing would end the agent.
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(&lines[1..], ["turn ended", "still open"], "{}", run.log);
    assert!(
        !run.output.contains("closed this session"),
        "{:?}",
        run.output
    );
}

#[test]
fn closes_without_moving_when_moves_are_off() {
    for (key, value) in [("claude.mode", "manual"), ("claude.relaunch", "off")] {
        let sb = two_accounts();
        sb.ok(&["config", "set", key, value]);
        close_idle(&sb, "2s");
        // The account is full: in auto mode with relaunch on this would move.
        let fake = IDLE.replace(
            "stop\necho \"turn ended\"",
            r#"printf '{"session_id":"%s","rate_limits":{"five_hour":{"used_percentage":95,"resets_at":4102444800}}}' $s | "$HATS" statusline > /dev/null
printf '{"session_id":"%s","transcript_path":"%s","hook_event_name":"StopFailure","error":"rate_limit"}' $s "$T" | "$HATS" hook stop
echo "turn ended""#,
        );
        let run = run_in_pty(&sb, &fake, |_| {});
        let lines: Vec<&str> = run.log.lines().collect();
        assert!(!lines[0].ends_with("parent=none"), "{key}: {}", run.log);
        assert_eq!(&lines[1..], ["turn ended", "term"], "{key}: {}", run.log);
        assert!(!run.log.contains("--resume"), "{key}: moved: {}", run.log);
        assert!(
            run.output.contains("claude --resume s-1"),
            "{key}: {:?}",
            run.output
        );
        let moved = sb
            .registry_lines()
            .into_iter()
            .any(|l| l["session"] == "s-1" && l["label"] == "side");
        assert!(!moved, "{key}: {:?}", sb.registry_lines());
    }
}

#[test]
fn a_moved_session_left_idle_closes_too() {
    let sb = two_accounts();
    close_idle(&sb, "3s");
    // The resumed agent waits like the first one; the clock goes on from the
    // turn that ended before the move.
    let fake = FAKE.replace(
        "echo resumed >> \"$FAKE_LOG\"; exit 0 ;;",
        "echo resumed >> \"$FAKE_LOG\"; trap 'echo term >> \"$FAKE_LOG\"; exit 143' TERM; while :; do sleep 0.05; done ;;",
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    assert!(run.log.ends_with("resumed\nterm\n"), "{}", run.log);
    assert!(
        run.output.contains("claude --resume s-1"),
        "{:?}",
        run.output
    );
    assert!(run.elapsed < Duration::from_secs(6), "{:?}", run.elapsed);
}
