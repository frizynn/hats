//! `hats run` stays as the parent of an interactive Claude Code session and
//! closes it once it went unused for `claude.close_idle`: SIGTERM, then a line
//! that says how to resume it. The fake `claude` below calls the real hats
//! hooks the way Claude Code does, then waits for SIGTERM.

mod common;

use std::fs;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::Sandbox;

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

/// `work` in the seat, with the hooks installed.
fn work() -> Sandbox {
    let sb = Sandbox::new();
    sb.signed_in("work", "org-work");
    sb.ok(&["use", "work"]);
    // The idle hooks say when the session is unused; setup installs them.
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
        .current_dir(&sb.home)
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
fn passes_the_agents_exit_status_through() {
    let sb = work();
    let fake = "#!/bin/sh\necho start >> \"$FAKE_LOG\"\nexit 7\n";
    let run = run_in_pty(&sb, fake, |_| {});
    assert_eq!(run.status.code(), Some(7));
}

#[test]
fn a_closed_terminal_ends_both() {
    let sb = work();
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
    let sb = work();
    sb.ok(&["setup", "--undo"]);
    let fake = "#!/bin/sh\necho \"start parent=${HATS_SUPERVISOR:-none}\" >> \"$FAKE_LOG\"\n";
    let run = run_in_pty(&sb, fake, |_| {});
    assert_eq!(run.log.trim(), "start parent=none");
}

#[test]
fn an_agent_that_ignores_sigterm_is_killed_after_ten_seconds() {
    let sb = work();
    close_idle(&sb, "1s");
    let fake = IDLE.replace(
        "trap 'echo term >> \"$FAKE_LOG\"; exit 143' TERM",
        "trap '' TERM",
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    assert!(run.log.ends_with("turn ended\n"), "{}", run.log);
    assert!(run.elapsed >= Duration::from_secs(11), "{:?}", run.elapsed);
    assert!(run.status.success(), "the session was closed");
    assert!(
        run.output.contains("claude --resume s-1"),
        "{:?}",
        run.output
    );
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
    let sb = work();
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
fn the_agent_ends_when_its_parent_is_killed() {
    let sb = work();
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
fn closes_a_session_left_idle_and_says_how_to_resume_it() {
    let sb = work();
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
    let run = sb.home.join(".local/state/hats/run");
    assert!(fs::read_dir(&run).map(|d| d.count() == 0).unwrap_or(true));
}

#[test]
fn never_closes_during_a_turn() {
    let sb = work();
    close_idle(&sb, "1s");
    // A turn, or a wait for approval inside one, longer than the limit:
    // after UserPromptSubmit and before Stop the agent is busy, also when it
    // compacts the conversation.
    let fake = later(
        r#"printf '{"session_id":"%s"}' $s | "$HATS" hook prompt
echo "long turn" >> "$FAKE_LOG"
printf '{"session_id":"%s","transcript_path":"%s","source":"compact"}' $s "$T" | "$HATS" hook session-start
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
    let sb = work();
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
    let sb = work();
    close_idle(&sb, "2s");
    // Something wakes the agent without UserPromptSubmit; the transcript
    // still grows. Without that write it would close at 2 s.
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
    let sb = work();
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
fn a_session_opened_and_never_used_closes_too() {
    let sb = work();
    close_idle(&sb, "2s");
    // SessionStart, then nothing: no prompt, no turn.
    let fake = r#"#!/bin/sh
echo "start args=$*" >> "$FAKE_LOG"
trap 'echo term >> "$FAKE_LOG"; exit 143' TERM
printf '{"session_id":"s-1","transcript_path":"%s","source":"startup"}' "$HOME/s-1.jsonl" | "$HATS" hook session-start
i=0
while [ $i -lt 100 ]; do sleep 0.05; i=$((i + 1)); done
echo "still open" >> "$FAKE_LOG"
"#;
    let run = run_in_pty(&sb, fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(&lines[1..], ["term"], "{}", run.log);
    assert!(
        run.output.contains("Resume it with: claude --resume s-1"),
        "{:?}",
        run.output
    );
    assert!(run.elapsed >= Duration::from_secs(2), "{:?}", run.elapsed);
}

#[test]
fn each_close_is_recorded_for_the_interactive_view() {
    let sb = work();
    close_idle(&sb, "2s");
    let run = run_in_pty(&sb, IDLE, |_| {});
    assert!(run.status.success());
    let closes = fs::read_to_string(sb.home.join(".local/state/hats/closes.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = closes
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{closes}");
    let line = lines[0].as_object().unwrap();
    let keys: Vec<&str> = line.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["v", "at", "harness", "session", "label", "idle", "cwd"],
        "{closes}"
    );
    assert_eq!(line["v"], 1);
    let at = line["at"].as_str().unwrap();
    assert!(
        at.len() == 24 && at.ends_with('Z') && at.as_bytes()[10] == b'T',
        "UTC ISO time: {at}"
    );
    assert_eq!(line["harness"], "claude");
    assert_eq!(line["session"], "s-1");
    assert_eq!(line["label"], "work");
    assert_eq!(line["idle"], "2s");
    assert_eq!(
        line["cwd"].as_str().unwrap(),
        sb.home.canonicalize().unwrap().to_str().unwrap()
    );
}

/// Starts a command the way Claude Code 2.1.302 starts a background task:
/// a shell child that sources the session's shell snapshot first. Every
/// session runs in the seat, `~/.claude`.
fn background(command: &str) -> String {
    format!(
        r#"snapshot="$HOME/.claude/shell-snapshots/snapshot-sh-1.sh"
mkdir -p "${{snapshot%/*}}" && : > "$snapshot"
/bin/sh -c "source $snapshot 2>/dev/null || true && eval '{command}' < /dev/null && pwd -P >| /dev/null" &
bg=$!"#
    )
}

#[test]
fn a_close_waits_for_a_background_shell_to_end() {
    let sb = work();
    close_idle(&sb, "3s");
    // Without it the session would close at 3 s, with the task running.
    let fake = IDLE.replace(
        "stop\necho \"turn ended\" >> \"$FAKE_LOG\"",
        &format!(
            "{}\nstop\necho \"turn ended\" >> \"$FAKE_LOG\"\nwait $bg\necho \"background done\" >> \"$FAKE_LOG\"",
            background("sleep 4")
        ),
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(
        &lines[1..],
        ["turn ended", "background done", "term"],
        "{}",
        run.log
    );
    assert!(
        run.output.contains("claude --resume s-1"),
        "{:?}",
        run.output
    );
}

#[test]
fn a_background_shell_holds_a_close_for_one_more_close_idle_at_most() {
    let sb = work();
    close_idle(&sb, "2s");
    // A task that never ends counts as use for 2 s of its run time; the
    // session then closes 2 s later, with the task still running.
    let fake = IDLE.replace(
        "stop\necho \"turn ended\"",
        &format!("{}\nstop\necho \"turn ended\"", background("sleep 8")),
    );
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().collect();
    assert_eq!(&lines[1..], ["turn ended", "term"], "{}", run.log);
    assert!(
        run.elapsed >= Duration::from_secs(4) && run.elapsed < Duration::from_secs(7),
        "{:?}",
        run.elapsed
    );
}

#[test]
fn a_session_started_inside_another_does_not_speak_for_it() {
    let sb = work();
    // In its turn the agent runs `claude -p`, through hats as the `claude`
    // function does. That session's turn end must not look like the outer
    // agent's: the outer agent is still in its turn, longer than the limit,
    // and must not close before it ends.
    close_idle(&sb, "1s");
    let fake = IDLE
        .replace(
            "trap 'echo term",
            r#"case " $* " in *" -p "*) printf '{"session_id":"nested","hook_event_name":"Stop"}' | "$HATS" hook stop; echo nested >> "$FAKE_LOG"; exit 0 ;; esac
trap 'echo term"#,
        )
        .replace(
            "stop\necho \"turn ended\"",
            "\"$HATS\" run -- -p hi < /dev/null > /dev/null\nsleep 2\nstop\necho \"turn ended\"",
        );
    let run = run_in_pty(&sb, &fake, |_| {});
    let lines: Vec<&str> = run.log.lines().filter(|l| !l.contains("args=-p")).collect();
    assert_eq!(&lines[1..], ["nested", "turn ended", "term"], "{}", run.log);
    assert!(run.elapsed >= Duration::from_secs(3), "{:?}", run.elapsed);
}
