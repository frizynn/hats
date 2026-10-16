//! `hats run` as a small parent of Claude Code, so a running session can move
//! to another account without touching credentials: when the account passes
//! the threshold or hits its limit, and the agent is idle, the parent ends
//! Claude Code with SIGTERM and starts it again with `--resume <session>` in
//! the next account's directory, in the same terminal.
//!
//! The parent shares the terminal and the process group with Claude Code and
//! never reads or writes the terminal. It sleeps in `sigwait(2)`; the session's
//! hooks and status line wake it with SIGUSR1 after writing to its run
//! directory (`<state>/run/<pid>/`):
//!
//! - `idle`: created by the `Stop` and `StopFailure` hooks, removed by
//!   `UserPromptSubmit`. A move happens only while it exists, so a turn is
//!   never cut, and without the hooks a session is never moved.
//! - `move.json`: written by the status line wrapper when the session's
//!   account passed the threshold, or by `StopFailure` on a rate limit.
//!
//! Measured on Claude Code 2.1.301: SIGTERM exits in about 0.26 s and resets
//! the terminal modes Claude Code set (bracketed paste, focus, mouse, kitty
//! keyboard), so the parent does not restore anything itself.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::account::{Account, Harness, create_private_dir};
use crate::auto;
use crate::config::{HarnessConfig, Mode, Relaunch};
use crate::policy::Reason;
use crate::registry::{self, Switch};
use crate::store::Store;

/// Set in the agent's environment: the pid of the `hats run` parent.
pub const PID_ENV: &str = "HATS_SUPERVISOR";
/// Seconds between deciding to move and SIGTERM: the transcript is written
/// asynchronously, and a prompt typed meanwhile cancels the move.
const GRACE_SECS: u32 = 1;
/// Seconds after SIGTERM before SIGKILL.
const KILL_SECS: u32 = 10;

#[derive(Serialize, Deserialize)]
struct MoveRequest {
    session: String,
    reason: Reason,
}

fn run_dir(store: &Store, pid: u32) -> PathBuf {
    store.state.join("run").join(pid.to_string())
}

/// The run directory of the `hats run` parent of this process, if any.
fn parent_dir(store: &Store) -> Option<(u32, PathBuf)> {
    let pid: u32 = std::env::var(PID_ENV).ok()?.parse().ok()?;
    let dir = run_dir(store, pid);
    dir.is_dir().then_some((pid, dir))
}

fn wake(pid: u32) {
    // SAFETY: kill(2) with a pid and a signal number.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR1) };
}

fn write_request(dir: &Path, session: &str, reason: Reason) -> io::Result<()> {
    let tmp = dir.join("move.json.tmp");
    let request = MoveRequest {
        session: session.to_owned(),
        reason,
    };
    fs::write(&tmp, serde_json::to_vec(&request).expect("plain struct"))?;
    fs::rename(tmp, dir.join("move.json"))
}

/// Status line: if the session's account passed the threshold and another
/// account has room, asks the parent to move the session.
pub fn request_move(
    store: &Store,
    config: &HarnessConfig,
    account: &Account,
    session: &str,
    now: SystemTime,
) -> Result<(), String> {
    let Some((pid, dir)) = parent_dir(store) else {
        return Ok(());
    };
    if config.mode != Mode::Auto
        || config.relaunch != Relaunch::Idle
        || dir.join("move.json").exists()
    {
        return Ok(());
    }
    let statuses = auto::statuses(store, account.harness, config)?;
    let Some((_, reason)) = auto::next_account(&statuses, &account.label, config, now, None) else {
        return Ok(());
    };
    write_request(&dir, session, reason).map_err(|e| e.to_string())?;
    wake(pid);
    Ok(())
}

/// `UserPromptSubmit` hook: the agent is working.
pub fn mark_busy(store: &Store) -> io::Result<()> {
    let Some((_, dir)) = parent_dir(store) else {
        return Ok(());
    };
    match fs::remove_file(dir.join("idle")) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `Stop` and `StopFailure` hooks: the agent is idle. A rate limit error asks
/// for a move even without status line data.
pub fn mark_idle(store: &Store, session: &str, rate_limited: bool) -> io::Result<()> {
    let Some((pid, dir)) = parent_dir(store) else {
        return Ok(());
    };
    fs::write(dir.join("idle"), b"")?;
    if rate_limited {
        write_request(&dir, session, Reason::Limit)?;
    }
    wake(pid);
    Ok(())
}

/// Claude Code arguments that select the conversation or start a new turn;
/// on a relaunch they are replaced by `--resume <session>`.
const DROP_WITH_VALUE: &[&str] = &["--session-id"];
const DROP_WITH_OPTIONAL_VALUE: &[&str] = &["-r", "--resume", "--from-pr", "--teleport"];
const DROP: &[&str] = &["-c", "--continue", "--fork-session"];
/// Kept with their single value (Claude Code 2.1.301 `--help`).
const WITH_VALUE: &[&str] = &[
    "--agent",
    "--agents",
    "--append-system-prompt",
    "--autocompact",
    "--debug-file",
    "--effort",
    "--environment",
    "--fallback-model",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--model",
    "-n",
    "--name",
    "--output-format",
    "--permission-mode",
    "--permission-prompt-tool",
    "--remote-control-session-name-prefix",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-snapshot",
];
/// Kept with every following value that does not start with `-`.
const WITH_VALUES: &[&str] = &[
    "--add-dir",
    "--allowedTools",
    "--allowed-tools",
    "--betas",
    "--disallowedTools",
    "--disallowed-tools",
    "--file",
    "--mcp-config",
    "--plugin-dir",
    "--plugin-url",
    "--tools",
];
/// Kept as they are; never followed by a value.
const FLAGS: &[&str] = &[
    "--allow-dangerously-skip-permissions",
    "--ax-screen-reader",
    "--bare",
    "--brief",
    "--chrome",
    "--dangerously-skip-permissions",
    "--disable-slash-commands",
    "--exclude-dynamic-system-prompt-sections",
    "--ide",
    "--no-chrome",
    "--restricted",
    "--safe-mode",
    "--strict-mcp-config",
    "--verbose",
];
/// Not an interactive session: run as a plain exec.
const NOT_INTERACTIVE: &[&str] = &["-p", "--print", "-h", "--help", "-v", "--version"];

/// The arguments for `claude --resume <session>` that keep the user's
/// options, or `None` when they cannot be read safely (a subcommand, print
/// mode, or an option this table does not know followed by a value).
pub fn relaunch_args(original: &[OsString], session: &str) -> Option<Vec<OsString>> {
    let mut kept: Vec<OsString> = Vec::new();
    let mut args = original.iter().peekable();
    let is_value = |a: Option<&&OsString>| a.is_some_and(|a| !a.to_string_lossy().starts_with('-'));
    let mut saw_prompt = false;
    while let Some(arg) = args.next() {
        let text = arg.to_str()?;
        let name = text.split_once('=').map_or(text, |(n, _)| n);
        let inline_value = text.contains('=');
        if NOT_INTERACTIVE.contains(&name) {
            return None;
        }
        if DROP.contains(&name) {
            continue;
        }
        if DROP_WITH_VALUE.contains(&name) {
            if !inline_value {
                args.next()?;
            }
            continue;
        }
        if DROP_WITH_OPTIONAL_VALUE.contains(&name) {
            if !inline_value && is_value(args.peek()) {
                args.next();
            }
            continue;
        }
        if WITH_VALUE.contains(&name) {
            kept.push(arg.clone());
            if !inline_value {
                kept.push(args.next()?.clone());
            }
            continue;
        }
        if WITH_VALUES.contains(&name) {
            kept.push(arg.clone());
            while is_value(args.peek()) {
                kept.push(args.next()?.clone());
            }
            continue;
        }
        if FLAGS.contains(&name) {
            kept.push(arg.clone());
            continue;
        }
        if text.starts_with('-') {
            // Unknown option: keep it only if it cannot be swallowing a value.
            if is_value(args.peek()) {
                return None;
            }
            kept.push(arg.clone());
            continue;
        }
        // The first word is either a subcommand (`claude mcp ...`) or the
        // initial prompt, which already ran and must not run again.
        if kept.is_empty() && !saw_prompt && SUBCOMMANDS.contains(&text) {
            return None;
        }
        saw_prompt = true;
    }
    kept.push("--resume".into());
    kept.push(session.into());
    Some(kept)
}

const SUBCOMMANDS: &[&str] = &[
    "agents",
    "auth",
    "config",
    "doctor",
    "gateway",
    "install",
    "mcp",
    "migrate-installer",
    "plugin",
    "plugins",
    "setup-token",
    "update",
    "upgrade",
];

/// Whether `hats run` should stay as the parent of this launch.
/// Whether `hats run` should stay as the parent of this launch: an
/// interactive Claude Code session, in auto mode, whose settings run the
/// hooks that say when the agent is idle.
pub fn should_supervise(account: &Account, config: &HarnessConfig, args: &[OsString]) -> bool {
    // SAFETY: isatty(3) on the standard descriptors.
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    account.harness == Harness::Claude
        && config.mode == Mode::Auto
        && config.relaunch == Relaunch::Idle
        && tty
        && relaunch_args(args, "probe").is_some()
        && idle_hooks_installed(&account.dir.join("settings.json"))
}

/// Whether `settings` runs `hats hook prompt` and `hats hook stop`.
fn idle_hooks_installed(settings: &Path) -> bool {
    let Ok(text) = fs::read_to_string(settings) else {
        return false;
    };
    text.contains(" hook prompt\"") && text.contains(" hook stop\"")
}

struct SignalSet(libc::sigset_t);

impl SignalSet {
    fn of(signals: &[libc::c_int]) -> Self {
        let mut set = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigemptyset initializes the set before sigaddset reads it.
        unsafe {
            libc::sigemptyset(set.as_mut_ptr());
            for s in signals {
                libc::sigaddset(set.as_mut_ptr(), *s);
            }
            Self(set.assume_init())
        }
    }

    fn block(&self) {
        // SAFETY: a valid set; the previous mask is not needed.
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &self.0, std::ptr::null_mut()) };
    }

    fn wait(&self) -> libc::c_int {
        let mut sig = 0;
        // SAFETY: a valid set of blocked signals and a valid out pointer.
        unsafe { libc::sigwait(&self.0, &mut sig) };
        sig
    }
}

enum ChildState {
    Running,
    Stopped,
    Exited(i32),
}

fn reap(child: &Child) -> ChildState {
    let mut status = 0;
    // SAFETY: waitpid on our own child with a valid out pointer.
    let pid = unsafe {
        libc::waitpid(
            child.id() as libc::pid_t,
            &mut status,
            libc::WNOHANG | libc::WUNTRACED,
        )
    };
    if pid <= 0 {
        return ChildState::Running;
    }
    if libc::WIFSTOPPED(status) {
        ChildState::Stopped
    } else if libc::WIFEXITED(status) {
        ChildState::Exited(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        ChildState::Exited(128 + libc::WTERMSIG(status))
    } else {
        ChildState::Running
    }
}

struct Move {
    to: Account,
    id: String,
    reason: Reason,
    session: String,
    term_sent: bool,
    killed: bool,
}

/// Runs Claude Code for `account` and moves it between accounts until it
/// exits on its own. Returns Claude Code's exit status.
pub fn supervise(
    store: &Store,
    config: &HarnessConfig,
    mut account: Account,
    mut args: Vec<OsString>,
) -> Result<i32, String> {
    let me = std::process::id();
    let dir = run_dir(store, me);
    create_private_dir(&dir).map_err(|e| e.to_string())?;
    let _cleanup = RemoveOnDrop(dir.clone());

    // Ctrl+C and Ctrl+\ belong to the agent; blocked here, they stay pending
    // and harmless. The rest are what this loop waits for.
    SignalSet::of(&[libc::SIGINT, libc::SIGQUIT]).block();
    let waited = SignalSet::of(&[
        libc::SIGCHLD,
        libc::SIGUSR1,
        libc::SIGALRM,
        libc::SIGHUP,
        libc::SIGTERM,
    ]);
    waited.block();
    // SIGCHLD is ignored by default, and the kernel drops an ignored signal
    // even while it is blocked, so sigwait would never see the agent exit
    // (measured on macOS). A no-op handler makes it a real, waitable signal.
    extern "C" fn noop(_: libc::c_int) {}
    // SAFETY: installs a handler that does nothing; it never runs while the
    // signal is blocked and taken by sigwait.
    unsafe { libc::signal(libc::SIGCHLD, noop as *const () as libc::sighandler_t) };

    let terminal = Terminal::save();

    loop {
        let mut cmd = Command::new(account.harness.program());
        cmd.args(&args).env(PID_ENV, me.to_string());
        account.select(&mut cmd);
        // The agent must get every signal: a blocked mask survives exec, and
        // std does not clear it on macOS (measured: SIGTERM never reached it).
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                let mut empty = MaybeUninit::<libc::sigset_t>::uninit();
                libc::sigemptyset(empty.as_mut_ptr());
                libc::pthread_sigmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut());
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", account.harness.program()))?;
        let mut moving: Option<Move> = None;

        let status = loop {
            match waited.wait() {
                libc::SIGCHLD => match reap(&child) {
                    ChildState::Exited(code) => break code,
                    // The agent suspended itself (Ctrl+Z in raw mode): stop
                    // too, so the shell gets the terminal back. `fg` continues
                    // the whole process group.
                    ChildState::Stopped => unsafe {
                        // SAFETY: stopping ourselves.
                        libc::kill(me as libc::pid_t, libc::SIGSTOP);
                    },
                    ChildState::Running => {}
                },
                libc::SIGUSR1 => {
                    if moving.is_none() && dir.join("idle").exists() {
                        moving = plan_move(store, config, &account, &dir);
                        if moving.is_some() {
                            // SAFETY: alarm(3) schedules SIGALRM, which is blocked and waited for.
                            unsafe { libc::alarm(GRACE_SECS) };
                        }
                    }
                }
                libc::SIGALRM => {
                    let Some(m) = moving.as_mut() else { continue };
                    if m.term_sent {
                        // SAFETY: our own child.
                        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
                        m.killed = true;
                    } else if !dir.join("idle").exists() {
                        // A prompt arrived during the grace period: try again
                        // at the next idle point.
                        let m = moving.take().expect("checked");
                        let _ = write_request(&dir, &m.session, m.reason);
                    } else {
                        m.term_sent = true;
                        // SAFETY: our own child; alarm as above.
                        unsafe {
                            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                            libc::alarm(KILL_SECS);
                        }
                    }
                }
                // The terminal closed or someone asked us to stop: pass it on
                // and leave when the agent does.
                sig => unsafe {
                    // SAFETY: our own child.
                    libc::kill(child.id() as libc::pid_t, sig);
                },
            }
        };
        // SAFETY: cancels a pending alarm.
        unsafe { libc::alarm(0) };

        match moving {
            Some(m) if m.term_sent => {
                let line = Switch::new(
                    SystemTime::now(),
                    m.to.harness,
                    &m.id,
                    &m.to.label,
                    &m.to.dir,
                    Some(&m.session),
                    Some(m.reason),
                );
                registry::append(&store.registry(), &line).map_err(|e| e.to_string())?;
                if m.killed {
                    terminal.restore();
                }
                args = relaunch_args(&args, &m.session).expect("checked before supervising");
                account = m.to;
                let _ = fs::remove_file(dir.join("idle"));
            }
            _ => return Ok(status),
        }
    }
}

fn plan_move(store: &Store, config: &HarnessConfig, account: &Account, dir: &Path) -> Option<Move> {
    let bytes = fs::read(dir.join("move.json")).ok()?;
    let _ = fs::remove_file(dir.join("move.json"));
    let request: MoveRequest = serde_json::from_slice(&bytes).ok()?;
    let statuses = auto::statuses(store, account.harness, config).ok()?;
    let forced = (request.reason == Reason::Limit).then_some(Reason::Limit);
    let (to, reason) =
        auto::next_account(&statuses, &account.label, config, SystemTime::now(), forced)?;
    Some(Move {
        to: to.account.clone(),
        id: to.id.clone()?,
        reason,
        session: request.session,
        term_sent: false,
        killed: false,
    })
}

/// The terminal settings from before the agent started. Claude Code resets
/// its own modes on SIGTERM; this is only for an agent that had to be killed.
struct Terminal(Option<libc::termios>);

impl Terminal {
    fn save() -> Self {
        let mut t = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the struct when it returns 0.
        let ok = unsafe { libc::tcgetattr(0, t.as_mut_ptr()) == 0 };
        // SAFETY: initialized when ok.
        Self(ok.then(|| unsafe { t.assume_init() }))
    }

    fn restore(&self) {
        if let Some(t) = &self.0 {
            // SAFETY: a termios read from this terminal.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        // Bracketed paste, focus events and mouse reporting off, kitty
        // keyboard flags popped, cursor shown.
        let reset =
            b"\x1b[?2004l\x1b[?1004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\x1b[?25h";
        let _ = io::Write::write_all(&mut io::stdout(), reset);
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn relaunch(list: &[&str]) -> Option<Vec<String>> {
        relaunch_args(&args(list), "S")
            .map(|v| v.into_iter().map(|a| a.into_string().unwrap()).collect())
    }

    #[test]
    fn keeps_options_and_resumes_the_session() {
        assert_eq!(relaunch(&[]).unwrap(), ["--resume", "S"]);
        assert_eq!(
            relaunch(&[
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--add-dir",
                "a",
                "b",
                "--verbose"
            ])
            .unwrap(),
            [
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--add-dir",
                "a",
                "b",
                "--verbose",
                "--resume",
                "S"
            ]
        );
        assert_eq!(
            relaunch(&["--effort=high", "-c"]).unwrap(),
            ["--effort=high", "--resume", "S"]
        );
    }

    #[test]
    fn drops_the_old_session_and_the_initial_prompt() {
        assert_eq!(
            relaunch(&["--resume", "OLD", "--model", "x"]).unwrap(),
            ["--model", "x", "--resume", "S"]
        );
        assert_eq!(relaunch(&["-r"]).unwrap(), ["--resume", "S"]);
        assert_eq!(
            relaunch(&["--session-id", "ID", "fix the bug"]).unwrap(),
            ["--resume", "S"]
        );
        assert_eq!(
            relaunch(&["--model", "x", "fix the bug"]).unwrap(),
            ["--model", "x", "--resume", "S"]
        );
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        assert_eq!(relaunch(&["-p", "hi"]), None);
        assert_eq!(relaunch(&["mcp", "list"]), None);
        assert_eq!(relaunch(&["--unknown-option", "value"]), None);
        assert!(relaunch(&["--unknown-flag"]).is_some());
    }
}
