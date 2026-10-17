//! `hats run` as a small parent of Claude Code, so a running session can move
//! to another account without touching credentials: when the user chooses
//! another account, or the session's account passes the threshold or hits
//! its limit, and the agent is idle, the parent ends Claude Code with SIGTERM
//! and starts it again with `--resume <session>` in the new account's
//! directory, in the same terminal. A session left unused for
//! `claude.close_idle` is ended the same way and not started again; the
//! parent prints how to resume it.
//!
//! The parent shares the terminal and the process group with Claude Code and
//! never reads or writes the terminal. It sleeps in `sigwait(2)`; the session's
//! hooks and status line, and `hats use`, wake it with SIGUSR1 after writing
//! to its run directory (`<state>/run/<pid>/`):
//!
//! - `turn`: when the agent last went quiet, with the session and its
//!   transcript: written by the `Stop` and `StopFailure` hooks when a turn
//!   ends and by `SessionStart` when the session opens, removed by
//!   `UserPromptSubmit`. While it is missing the agent is in a turn, maybe
//!   waiting for approval, and is never moved or closed; without the hooks a
//!   session is never moved. The session is unused from the later of that
//!   time and the transcript's last write, in case something wakes the agent
//!   without `UserPromptSubmit` (on 2.1.302 a finished background task does
//!   run it). A turn cut with Esc runs no hook (measured on 2.1.302, also no
//!   `idle_prompt` notification), so it stays a turn.
//! - `idle`: the time a turn ended, written and removed with `turn`, for a
//!   parent from before `turn`.
//! - `move.json`: written by the status line wrapper when the session's
//!   account passed the threshold, by `StopFailure` on a rate limit, and by
//!   `hats use` with the account the user chose.
//! - `account`: the label of the account the agent runs on, for `hats use`.
//!
//! A move the user asked for starts as soon as the agent is idle: they chose
//! elsewhere, so they are not typing here. One the limits asked for starts
//! right after a turn ends, or once the session has been unused for
//! `UNUSED_MS`, since in between the user may be typing the next prompt, and
//! a prompt typed and not sent is lost (measured on Claude Code 2.1.302:
//! after SIGTERM and `--resume` the prompt is empty and no file holds it).
//!
//! A background shell still running holds a move until the next turn end and
//! counts as use for a close, for at most `close_idle` of its run time (see
//! `background`): either would end it with Claude Code.
//!
//! Measured on Claude Code 2.1.301: SIGTERM exits in about 0.26 s and resets
//! the terminal modes Claude Code set (bracketed paste, focus, mouse, kitty
//! keyboard). The parent restores the terminal only when the agent dies of a
//! signal it could not handle.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::account::{Account, Harness, create_private_dir};
use crate::auto;
use crate::background;
use crate::config::{self, Config, HarnessConfig, Mode, Relaunch};
use crate::limits::unix_millis;
use crate::policy::Reason;
use crate::process;
use crate::registry::{self, Close, Switch};
use crate::relaunch;
use crate::store::Store;

/// Set in the agent's environment: the pid of the `hats run` parent.
pub const PID_ENV: &str = "HATS_SUPERVISOR";
/// Seconds between deciding to move and SIGTERM: the transcript is written
/// asynchronously, and a prompt typed meanwhile cancels the move.
const GRACE_SECS: u32 = 1;
/// Seconds after SIGTERM before SIGKILL.
const KILL_SECS: u32 = 10;
/// How long after a turn ends a move the limits asked for may start at once.
/// Measured on Claude Code 2.1.301: the status line, which asks for the move,
/// runs 0.31 to 0.34 s after the `Stop` hook (its 300 ms debounce), and the
/// wrapper asks before running the user's own command.
const MOVE_WINDOW_MS: u64 = 2000;
/// A session unused this long is taken as left alone, so a move the limits
/// asked for may start even though no turn just ended.
const UNUSED_MS: u64 = 60_000;
/// Longest wait between checks of the clock for closing. The clock is the
/// wall clock, which counts time the computer sleeps; alarm(3) may not (on
/// macOS it is not documented, and `mach_absolute_time` stops in sleep), so
/// after a sleep a due session closes at most this late.
const CHECK_SECS: u64 = 15 * 60;

#[derive(Serialize, Deserialize)]
struct MoveRequest {
    /// The session to resume; `turn` has it when `hats use` asks. A parent
    /// from before `hats use` asked reads only requests that have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    reason: Reason,
    /// The account the user chose, when they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to: Option<String>,
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

/// Replaces `name` in `dir` at once, so the parent never reads half of it.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, dir.join(name))
}

fn write_request(dir: &Path, request: &MoveRequest) -> io::Result<()> {
    write_file(
        dir,
        "move.json",
        &serde_json::to_vec(request).expect("plain struct"),
    )
}

/// What the limits ask for: a move of `session` off its account.
fn limits_request(session: &str, reason: Reason) -> MoveRequest {
    MoveRequest {
        session: Some(session.to_owned()),
        reason,
        to: None,
    }
}

/// Whether running sessions move to another account.
fn moves(config: &HarnessConfig) -> bool {
    config.mode == Mode::Auto && config.relaunch == Relaunch::Idle
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
    if !moves(config) || dir.join("move.json").exists() {
        return Ok(());
    }
    let statuses = auto::statuses(store, account.harness, config)?;
    let Some((_, reason)) = auto::next_account(&statuses, &account.label, config, now, None) else {
        return Ok(());
    };
    write_request(&dir, &limits_request(session, reason)).map_err(|e| e.to_string())?;
    wake(pid);
    Ok(())
}

/// A choice of default account by the user: asks every `hats run` parent
/// whose agent runs on another account to move it to `to`, at once if the
/// agent is idle, else when its turn ends. Returns how many were asked.
pub fn move_running(store: &Store, config: &HarnessConfig, to: &Account) -> usize {
    if to.harness != Harness::Claude || config.relaunch != Relaunch::Idle {
        return 0;
    }
    let Some(me) = process::info(std::process::id()) else {
        return 0;
    };
    let request = MoveRequest {
        session: None,
        reason: Reason::Manual,
        to: Some(to.label.clone()),
    };
    parents(store)
        .into_iter()
        .filter(|(pid, dir)| {
            // SIGUSR1 ends a process that does not handle it: never send it
            // to a program that took the pid of a parent that is gone.
            if !process::info(*pid).is_some_and(|p| p.name == me.name) {
                return false;
            }
            // A parent from before `hats use` moved sessions writes none.
            let Ok(on) = fs::read(dir.join("account")) else {
                return false;
            };
            if on == to.label.as_bytes() {
                drop_choice(dir);
                return false;
            }
            write_request(dir, &request).is_ok()
        })
        .inspect(|(pid, _)| wake(*pid))
        .count()
}

/// The user chose the account the agent of `dir` already runs on: an
/// earlier choice still waiting for its turn to end would move it away.
fn drop_choice(dir: &Path) {
    let path = dir.join("move.json");
    let earlier = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<MoveRequest>(&bytes).ok());
    if earlier.is_some_and(|r| r.reason == Reason::Manual) {
        let _ = fs::remove_file(path);
    }
}

/// The run directories of the `hats run` parents still running, with their
/// pids. Removes those of parents that are gone: one killed outright, or
/// from before this cleanup, leaves its directory behind.
fn parents(store: &Store) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = fs::read_dir(store.state.join("run")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            if process::alive(pid) {
                return Some((pid, entry.path()));
            }
            let _ = fs::remove_dir_all(entry.path());
            None
        })
        .collect()
}

/// Removes the run directories of `hats run` parents that are gone.
pub fn remove_stale(store: &Store) {
    parents(store);
}

/// Interactive Claude Code sessions of this user with no `hats run` parent:
/// nothing moves or closes them until they are opened again through hats.
/// Reads the kernel's names, parents and arguments, never the environment.
pub fn unsupervised() -> usize {
    let Some(hats) = process::info(std::process::id()) else {
        return 0;
    };
    process::all()
        .into_iter()
        .filter(|&pid| runs_without(pid, &hats))
        .count()
}

/// Whether `pid` is an interactive Claude Code session of the user running
/// `hats`, whose parent is not a process of the same name.
fn runs_without(pid: u32, hats: &process::Info) -> bool {
    process::info(pid).is_some_and(|p| {
        p.uid == hats.uid
            && p.tty
            && may_be_claude(&p.name)
            && process::args(pid).is_some_and(|a| is_claude_session(&a))
            && process::info(p.ppid).is_none_or(|parent| parent.name != hats.name)
    })
}

/// The kernel's name of a Claude Code process: the native install runs a
/// file named after its version (`2.1.302`), npm's runs `node`.
fn may_be_claude(name: &[u8]) -> bool {
    name == b"claude"
        || name == b"node"
        || (!name.is_empty() && name.iter().all(|b| b.is_ascii_digit() || *b == b'.'))
}

/// `claude [options]`, or `node …/claude [options]`, that hats would keep a
/// parent for: no subcommand or print mode. From NUL-separated arguments.
fn is_claude_session(args: &[u8]) -> bool {
    let named = |arg: Option<&[u8]>, name: &[u8]| {
        arg.is_some_and(|a| a.rsplit(|b| *b == b'/').next() == Some(name))
    };
    let mut args = args.split(|b| *b == 0);
    let first = args.next();
    if !named(first, b"claude") && !(named(first, b"node") && named(args.next(), b"claude")) {
        return false;
    }
    let rest: Vec<OsString> = args.map(|a| OsStr::from_bytes(a).to_owned()).collect();
    relaunch::args(&rest, "probe").is_some()
}

/// When the agent last went quiet: its turn ended, or the session opened. The
/// file `turn` holds it as three lines: that time in Unix milliseconds, the
/// session, the transcript.
pub struct Turn {
    session: String,
    transcript: Option<PathBuf>,
    ended: u64,
}

impl Turn {
    pub fn new(session: String, transcript: Option<PathBuf>, ended: SystemTime) -> Self {
        Self {
            session,
            transcript,
            ended: unix_millis(ended),
        }
    }

    fn write(&self, dir: &Path) -> io::Result<()> {
        let mut bytes = format!("{}\n{}\n", self.ended, self.session).into_bytes();
        if let Some(t) = &self.transcript {
            bytes.extend_from_slice(t.as_os_str().as_bytes());
        }
        write_file(dir, "turn", &bytes)
    }

    /// When the session was last used: the later of `ended` and the
    /// transcript's last write.
    fn used(&self) -> SystemTime {
        let ended = UNIX_EPOCH + Duration::from_millis(self.ended);
        let written = self
            .transcript
            .as_ref()
            .and_then(|t| fs::metadata(t).and_then(|m| m.modified()).ok());
        written.map_or(ended, |w| w.max(ended))
    }

    fn read(dir: &Path) -> Option<Self> {
        let bytes = fs::read(dir.join("turn")).ok()?;
        let mut lines = bytes.splitn(3, |b| *b == b'\n');
        let ended = std::str::from_utf8(lines.next()?).ok()?.parse().ok()?;
        let session = String::from_utf8(lines.next()?.to_vec()).ok()?;
        let transcript = lines
            .next()
            .filter(|t| !t.is_empty())
            .map(|t| PathBuf::from(OsStr::from_bytes(t)));
        Some(Self {
            session,
            transcript,
            ended,
        })
    }
}

/// `UserPromptSubmit` hook: the agent is working.
pub fn mark_busy(store: &Store) -> io::Result<()> {
    let Some((_, dir)) = parent_dir(store) else {
        return Ok(());
    };
    // `turn` first: the clock for closing stops before anything else.
    // Both go even if one fails: either one left could end the agent.
    let gone = |name| match fs::remove_file(dir.join(name)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    let turn = gone("turn");
    gone("idle").and(turn)
}

/// `SessionStart` hook: the session opened and waits for its first prompt.
pub fn mark_opened(store: &Store, turn: &Turn) -> io::Result<()> {
    let Some((pid, dir)) = parent_dir(store) else {
        return Ok(());
    };
    turn.write(&dir)?;
    wake(pid);
    Ok(())
}

/// `Stop` and `StopFailure` hooks: the agent is idle. A rate limit error asks
/// for a move even without status line data, when sessions move.
pub fn mark_idle(
    store: &Store,
    config: &HarnessConfig,
    turn: &Turn,
    rate_limited: bool,
) -> io::Result<()> {
    let Some((pid, dir)) = parent_dir(store) else {
        return Ok(());
    };
    turn.write(&dir)?;
    // A parent from before `turn` reads only this.
    fs::write(dir.join("idle"), turn.ended.to_string())?;
    if rate_limited && moves(config) {
        write_request(&dir, &limits_request(&turn.session, Reason::Limit))?;
    }
    wake(pid);
    Ok(())
}

/// Whether `hats run` should stay as the parent of this launch: an
/// interactive Claude Code session that may move or close, whose settings
/// run the hooks that say when the agent is idle.
pub fn should_supervise(account: &Account, config: &Config, args: &[OsString]) -> bool {
    // SAFETY: isatty(3) on the standard descriptors.
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    account.harness == Harness::Claude
        && (moves(&config.claude) || config.close_idle.is_some())
        && tty
        && relaunch::args(args, "probe").is_some()
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
    /// Ended by a signal it did not handle, so it could not reset the terminal.
    Killed(i32),
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
        ChildState::Killed(libc::WTERMSIG(status))
    } else {
        ChildState::Running
    }
}

/// Why the parent ends the agent.
enum Plan {
    /// Start it again on another account, resuming the session.
    Move {
        to: Account,
        id: String,
        reason: Reason,
        session: String,
    },
    /// The session went unused this long: close it.
    Close { after: Duration, session: String },
    /// A signal was passed on to the agent: no move or close, leave when it
    /// does.
    Leave,
}

struct Ending {
    plan: Plan,
    term_sent: bool,
}

/// Runs Claude Code for `account`, moves it between accounts and closes it
/// when unused, until it exits. Returns Claude Code's exit status, or 0 when
/// the session was closed.
pub fn supervise(
    store: &Store,
    config: &Config,
    mut account: Account,
    mut args: Vec<OsString>,
) -> Result<i32, String> {
    // `hats config set` reaches running sessions at their next check.
    let current = || Config::load(&store.config).unwrap_or_else(|_| config.clone());
    let me = std::process::id();
    remove_stale(store);
    let dir = fresh_run_dir(store, me).map_err(|e| e.to_string())?;
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
    let sentinel = Sentinel::spawn();

    loop {
        // `hats use` asks only a parent on another account to move.
        let _ = write_file(&dir, "account", account.label.as_bytes());
        let mut cmd = Command::new(account.harness.program());
        cmd.args(&args).env(PID_ENV, me.to_string());
        account.select(&mut cmd);
        // The agent must get every signal: a blocked mask survives exec, and
        // std does not clear it on macOS (measured: SIGTERM never reached it).
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                unblock_all_signals();
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("could not start {}: {e}", account.harness.program()))?;
        if let Some(sentinel) = &sentinel {
            sentinel.watch(child.id());
        }
        let mut ending: Option<Ending> = None;
        // After a move the clock goes on from the last turn.
        schedule(
            &dir,
            child.id(),
            current().close_idle,
            None,
            SystemTime::now(),
        );

        let status = loop {
            match waited.wait() {
                libc::SIGCHLD => match reap(&child) {
                    ChildState::Exited(code) => break code,
                    ChildState::Killed(signal) => {
                        terminal.restore();
                        break 128 + signal;
                    }
                    // The agent suspended itself (Ctrl+Z in raw mode): stop
                    // too, so the shell gets the terminal back. `fg` continues
                    // the whole process group.
                    ChildState::Stopped => unsafe {
                        // SAFETY: stopping ourselves.
                        libc::kill(me as libc::pid_t, libc::SIGSTOP);
                    },
                    ChildState::Running => {}
                },
                // Something was written to the run directory, or it is time
                // to check the clock.
                libc::SIGUSR1 | libc::SIGALRM if ending.is_none() => {
                    ending = check(store, &current(), &account, &dir, &child);
                }
                libc::SIGUSR1 => {}
                libc::SIGALRM => match ending.as_mut() {
                    None
                    | Some(Ending {
                        plan: Plan::Leave, ..
                    }) => {}
                    Some(e) if e.term_sent => {
                        // SAFETY: our own child.
                        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
                    }
                    // The end of a move's grace period.
                    Some(e) => {
                        let config = current();
                        let hold = config.close_idle.unwrap_or(config::CLOSE_IDLE);
                        if dir.join("turn").exists()
                            && !shell_running(child.id(), hold, SystemTime::now())
                        {
                            terminate(&child, e);
                            continue;
                        }
                        // A prompt arrived, or a background shell is still
                        // running: try again at the next idle point.
                        if let Some(Ending {
                            plan:
                                Plan::Move {
                                    to,
                                    reason,
                                    session,
                                    ..
                                },
                            ..
                        }) = ending.take()
                        {
                            let request = MoveRequest {
                                session: Some(session),
                                reason,
                                to: (reason == Reason::Manual).then_some(to.label),
                            };
                            let _ = write_request(&dir, &request);
                        }
                        schedule(&dir, child.id(), config.close_idle, None, SystemTime::now());
                    }
                },
                // The terminal closed or someone asked us to stop: pass it on,
                // drop any move or close and leave when the agent does.
                sig => {
                    ending = Some(Ending {
                        plan: Plan::Leave,
                        term_sent: false,
                    });
                    // SAFETY: cancels a pending alarm; our own child.
                    unsafe {
                        libc::alarm(0);
                        libc::kill(child.id() as libc::pid_t, sig);
                    }
                }
            }
        };
        // SAFETY: cancels a pending alarm.
        unsafe { libc::alarm(0) };

        match ending {
            Some(Ending {
                plan:
                    Plan::Move {
                        to,
                        id,
                        reason,
                        session,
                    },
                term_sent: true,
            }) => {
                let line = Switch::new(
                    SystemTime::now(),
                    to.harness,
                    &id,
                    &to.label,
                    &to.dir,
                    Some(&session),
                    Some(reason),
                );
                registry::append(&store.registry(), &line).map_err(|e| e.to_string())?;
                args = relaunch::args(&args, &session).expect("checked before supervising");
                account = to;
            }
            Some(Ending {
                plan: Plan::Close { after, session },
                term_sent: true,
            }) => {
                let idle = config::show_close_idle(Some(after));
                let cwd = std::env::current_dir().unwrap_or_default();
                let line = Close::new(
                    SystemTime::now(),
                    &session,
                    &account.label,
                    idle.clone(),
                    &cwd,
                );
                // Both are a courtesy: the session is closed either way, and a
                // closed terminal must not turn into a crash.
                let _ = registry::append(&store.closes(), &line);
                let _ = writeln!(
                    io::stderr(),
                    "hats: closed this session after {idle} without use. Resume it with: claude --resume {session}"
                );
                return Ok(0);
            }
            _ => return Ok(status),
        }
    }
}

/// SIGTERM now, SIGKILL after `KILL_SECS` if the agent is still there.
fn terminate(child: &Child, ending: &mut Ending) {
    ending.term_sent = true;
    // SAFETY: our own child; alarm(3) schedules SIGALRM, which is blocked
    // and waited for.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        libc::alarm(KILL_SECS);
    }
}

/// Until when a background shell that started at `started` counts as use:
/// while it runs, for at most `hold` of its run time, so one that never ends
/// does not hold the session forever.
fn in_use_until(started: SystemTime, hold: Duration, now: SystemTime) -> SystemTime {
    started.checked_add(hold).map_or(now, |end| end.min(now))
}

/// Whether `agent` runs a background shell that still counts as use.
fn shell_running(agent: u32, hold: Duration, now: SystemTime) -> bool {
    background::newest(agent).is_some_and(|started| in_use_until(started, hold, now) >= now)
}

/// How long until the session closes, and its id, if the agent is idle:
/// `after` from its last use, the later of `Turn::used` and the use of the
/// newest background shell, which started at `shell`. Zero when it is due.
fn time_to_close(
    dir: &Path,
    after: Duration,
    shell: Option<SystemTime>,
    now: SystemTime,
) -> Option<(Duration, String)> {
    let turn = Turn::read(dir)?;
    let running = shell.map(|started| in_use_until(started, after, now));
    let used = running.map_or(turn.used(), |r| r.max(turn.used()));
    let left = used.checked_add(after).map_or(Duration::MAX, |due| {
        due.duration_since(now).unwrap_or_default()
    });
    Some((left, turn.session))
}

/// Sets the alarm for the next check: when the session is due to close or,
/// `move_in` from now, a move held back may start; at most `CHECK_SECS`
/// away. Clears it when there is nothing to wait for: the agent is busy, or
/// closing is off and no move waits.
fn schedule(
    dir: &Path,
    agent: u32,
    after: Option<Duration>,
    move_in: Option<Duration>,
    now: SystemTime,
) {
    let close = after
        .and_then(|after| time_to_close(dir, after, background::newest(agent), now))
        .map(|(left, _)| left);
    let secs = match close.into_iter().chain(move_in).min() {
        Some(left) => {
            let whole = left
                .as_secs()
                .saturating_add(u64::from(left.subsec_nanos() > 0));
            whole.clamp(1, CHECK_SECS) as u32
        }
        None => 0,
    };
    // SAFETY: alarm(3) schedules SIGALRM, which is blocked and waited for.
    unsafe { libc::alarm(secs) };
}

/// At a wake or a check of the clock: starts a move if one was asked for and
/// may start now, else ends the agent if the session went unused for
/// `close_idle`, else sets the next check.
fn check(
    store: &Store,
    config: &Config,
    account: &Account,
    dir: &Path,
    child: &Child,
) -> Option<Ending> {
    let now = SystemTime::now();
    let move_in = match plan_move(store, &config.claude, account, dir, now) {
        Next::Start(ending) => {
            // SAFETY: alarm(3) schedules SIGALRM, which is blocked and waited for.
            unsafe { libc::alarm(GRACE_SECS) };
            return Some(ending);
        }
        Next::After(wait) => Some(wait),
        Next::Nothing => None,
    };
    let after = config.close_idle;
    if let Some(after) = after
        && let Some((left, session)) =
            time_to_close(dir, after, background::newest(child.id()), now)
        && left.is_zero()
    {
        let mut ending = Ending {
            plan: Plan::Close { after, session },
            term_sent: false,
        };
        terminate(child, &mut ending);
        return Some(ending);
    }
    schedule(dir, child.id(), after, move_in, now);
    None
}

/// What becomes of the move asked for in `move.json`.
enum Next {
    Start(Ending),
    /// It may start then if the session stays unused.
    After(Duration),
    /// None was asked for, it waits for the agent's turn to end, or it was
    /// dropped: moves are off, or there is no account to move to.
    Nothing,
}

fn plan_move(
    store: &Store,
    config: &HarnessConfig,
    account: &Account,
    dir: &Path,
    now: SystemTime,
) -> Next {
    let Ok(bytes) = fs::read(dir.join("move.json")) else {
        return Next::Nothing;
    };
    let discard = || {
        let _ = fs::remove_file(dir.join("move.json"));
        Next::Nothing
    };
    let Ok(request) = serde_json::from_slice::<MoveRequest>(&bytes) else {
        return discard();
    };
    let chosen = request.reason == Reason::Manual;
    let allowed = if chosen {
        config.relaunch == Relaunch::Idle
    } else {
        moves(config)
    };
    if !allowed {
        return discard();
    }
    // In a turn: `Stop` wakes this parent again when it ends.
    let Some(turn) = Turn::read(dir) else {
        return Next::Nothing;
    };
    let unused = unix_millis(now).saturating_sub(unix_millis(turn.used()));
    if !chosen && (MOVE_WINDOW_MS..UNUSED_MS).contains(&unused) {
        return Next::After(Duration::from_millis(UNUSED_MS - unused));
    }
    let destination = if chosen {
        chosen_account(store, account, request.to.as_deref())
    } else {
        next_account(store, config, account, request.reason, now)
    };
    discard();
    let Some((to, id, reason)) = destination else {
        return Next::Nothing;
    };
    Next::Start(Ending {
        plan: Plan::Move {
            to,
            id,
            reason,
            session: request.session.unwrap_or(turn.session),
        },
        term_sent: false,
    })
}

/// The account the user chose, with its id, if it is another and signed in.
fn chosen_account(
    store: &Store,
    from: &Account,
    label: Option<&str>,
) -> Option<(Account, String, Reason)> {
    let to = Account::find(from.harness, label?, store).ok()?;
    let id = to.provider_id(store).ok()??;
    (to.dir != from.dir).then_some((to, id, Reason::Manual))
}

/// The account the switching policy moves `from` to, with its id, if `from`
/// must leave: a limit hit forces it.
fn next_account(
    store: &Store,
    config: &HarnessConfig,
    from: &Account,
    reason: Reason,
    now: SystemTime,
) -> Option<(Account, String, Reason)> {
    let statuses = auto::statuses(store, from.harness, config).ok()?;
    let forced = (reason == Reason::Limit).then_some(Reason::Limit);
    let (to, reason) = auto::next_account(&statuses, &from.label, config, now, forced)?;
    Some((to.account.clone(), to.id.clone()?, reason))
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

/// This parent's run directory, emptied of what a parent with the same pid
/// may have left behind if it was killed: a stale `move.json` would resume
/// the wrong session.
fn fresh_run_dir(store: &Store, pid: u32) -> io::Result<PathBuf> {
    let dir = run_dir(store, pid);
    match fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Async-signal-safe: callable between fork and exec.
fn unblock_all_signals() {
    let mut empty = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set before it is used.
    unsafe {
        libc::sigemptyset(empty.as_mut_ptr());
        libc::pthread_sigmask(libc::SIG_SETMASK, empty.as_ptr(), std::ptr::null_mut());
    }
}

/// A child that ends the agent if this parent dies without cleaning up
/// (SIGKILL, a crash): otherwise the orphaned agent keeps running in the
/// background, cannot read the terminal and still writes to it (measured
/// with Claude Code 2.1.301). It sleeps in read(2) on a pipe that only this
/// parent holds open and that carries the pid of each agent it starts; when
/// the parent is gone the read returns end-of-file, and the sentinel sends
/// the last agent SIGHUP, as a closed terminal would, and SIGCONT in case it
/// was suspended.
struct Sentinel {
    pid: libc::pid_t,
    pipe: fs::File,
}

impl Sentinel {
    fn spawn() -> Option<Self> {
        let mut fds = [0; 2];
        // SAFETY: pipe(2) fills both descriptors on success; the agent must
        // not inherit them, or the pipe would never close.
        let (read, write) = unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return None;
            }
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            (fs::File::from_raw_fd(fds[0]), fs::File::from_raw_fd(fds[1]))
        };
        // SAFETY: this process has one thread; the child never returns.
        match unsafe { libc::fork() } {
            -1 => None,
            0 => {
                drop(write);
                sentinel_main(&read)
            }
            pid => Some(Self { pid, pipe: write }),
        }
    }

    fn watch(&self, agent: u32) {
        // Four bytes, so one atomic write; if the sentinel is gone there is
        // nothing to do about it.
        let _ = (&self.pipe).write_all(&agent.to_ne_bytes());
    }
}

fn sentinel_main(pipe: &fs::File) -> ! {
    // SAFETY: plain signal-disposition and process calls in our own child.
    unsafe {
        // Ctrl+C and Ctrl+\ reach the whole process group; they are the agent's.
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        unblock_all_signals();
        let mut agent = 0;
        let mut buf = [0u8; 4];
        loop {
            match (&*pipe).read_exact(&mut buf) {
                Ok(()) => agent = u32::from_ne_bytes(buf),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(_) => libc::_exit(1),
            }
        }
        if agent > 0 {
            libc::kill(agent as libc::pid_t, libc::SIGHUP);
            libc::kill(agent as libc::pid_t, libc::SIGCONT);
        }
        libc::_exit(0)
    }
}

impl Drop for Sentinel {
    fn drop(&mut self) {
        // Killed before the pipe closes, so it never signals; reaped so it
        // does not linger as a zombie.
        // SAFETY: our own child.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            libc::waitpid(self.pid, std::ptr::null_mut(), 0);
        }
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

    #[test]
    fn a_run_directory_starts_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::from_env(|k| (k == "HOME").then(|| tmp.path().into())).unwrap();
        let stale = run_dir(&store, 42);
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("idle"), b"").unwrap();
        fs::write(
            stale.join("move.json"),
            br#"{"session":"old","reason":"limit"}"#,
        )
        .unwrap();

        let dir = fresh_run_dir(&store, 42).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    fn signed_in(store: &Store, label: &str, used: f64) -> Account {
        let account = Account::managed(Harness::Claude, label, store).unwrap();
        account.create(store).unwrap();
        fs::write(
            account.dir.join(".claude.json"),
            format!(r#"{{"oauthAccount":{{"accountUuid":"{label}","organizationUuid":"o"}}}}"#),
        )
        .unwrap();
        let id = account.provider_id(store).unwrap().unwrap();
        let window = crate::limits::LimitWindow {
            name: "five_hour".to_owned(),
            used_percent: used,
            resets_at: crate::limits::unix_secs(SystemTime::now()) + 3600,
        };
        crate::limits::save(store, &account, &id, vec![window], SystemTime::now()).unwrap();
        account
    }

    #[test]
    fn a_move_waits_for_the_session_to_be_left_alone_unless_the_user_chose() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::from_env(|k| (k == "HOME").then(|| tmp.path().into())).unwrap();
        let work = signed_in(&store, "work", 95.0);
        signed_in(&store, "side", 10.0);
        let dir = tmp.path().join("run");
        fs::create_dir_all(&dir).unwrap();
        let now = UNIX_EPOCH + Duration::from_millis(unix_millis(SystemTime::now()));
        let sec = Duration::from_secs(1);
        let limits = limits_request("s-1", Reason::Threshold);
        let chosen = |label: &str| MoveRequest {
            session: None,
            reason: Reason::Manual,
            to: Some(label.to_owned()),
        };
        // What becomes of `request` when the session went quiet `ago`
        // (`None`: in a turn): where it moves, or after how long it may.
        let plan = |config: &HarnessConfig, request: &MoveRequest, ago: Option<Duration>| {
            write_request(&dir, request).unwrap();
            let _ = fs::remove_file(dir.join("turn"));
            if let Some(ago) = ago {
                Turn::new("s-1".into(), None, now - ago)
                    .write(&dir)
                    .unwrap();
            }
            let next = match plan_move(&store, config, &work, &dir, now) {
                Next::Start(Ending {
                    plan:
                        Plan::Move {
                            to,
                            reason,
                            session,
                            ..
                        },
                    ..
                }) => format!("{} {reason:?} {session}", to.label),
                Next::After(wait) => format!("after {wait:?}"),
                Next::Start(_) => unreachable!("only moves are planned"),
                Next::Nothing => "nothing".to_owned(),
            };
            let kept = dir.join("move.json").exists();
            (next, kept)
        };
        let auto = HarnessConfig::default();
        let start = |to: &str, why: &str| (format!("{to} {why} s-1"), false);

        assert_eq!(
            plan(&auto, &limits, Some(sec / 2)),
            start("side", "Threshold")
        );
        // The user may be typing the next prompt: it would be lost.
        assert_eq!(
            plan(&auto, &limits, Some(30 * sec)),
            ("after 30s".into(), true)
        );
        assert_eq!(
            plan(&auto, &limits, Some(61 * sec)),
            start("side", "Threshold")
        );
        assert_eq!(plan(&auto, &limits, None), ("nothing".into(), true), "busy");
        // The user chose elsewhere: at once, but never during a turn.
        assert_eq!(
            plan(&auto, &chosen("side"), Some(30 * sec)),
            start("side", "Manual")
        );
        assert_eq!(plan(&auto, &chosen("side"), None), ("nothing".into(), true));
        assert_eq!(
            plan(&auto, &chosen("work"), Some(sec)),
            ("nothing".into(), false)
        );
        assert_eq!(
            plan(&auto, &chosen("gone"), Some(sec)),
            ("nothing".into(), false)
        );

        // Manual mode: only the user's choice moves; relaunch off: nothing.
        let manual = HarnessConfig {
            mode: Mode::Manual,
            ..HarnessConfig::default()
        };
        assert_eq!(
            plan(&manual, &limits, Some(sec / 2)),
            ("nothing".into(), false)
        );
        assert_eq!(
            plan(&manual, &chosen("side"), Some(sec)),
            start("side", "Manual")
        );
        let off = HarnessConfig {
            relaunch: Relaunch::Off,
            ..HarnessConfig::default()
        };
        assert_eq!(
            plan(&off, &chosen("side"), Some(sec)),
            ("nothing".into(), false)
        );
        assert_eq!(move_running(&store, &off, &work), 0);
    }

    #[test]
    fn removes_only_the_run_directories_of_parents_that_are_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::from_env(|k| (k == "HOME").then(|| tmp.path().into())).unwrap();
        let mut gone = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        gone.wait().unwrap();
        let live = run_dir(&store, std::process::id());
        let dead = run_dir(&store, gone.id());
        for dir in [&live, &dead] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("idle"), b"1").unwrap();
        }
        fs::create_dir_all(store.state.join("run/not-a-pid")).unwrap();

        remove_stale(&store);
        assert!(live.join("idle").exists(), "a live parent keeps its own");
        assert!(!dead.exists());
        assert!(store.state.join("run/not-a-pid").exists(), "not ours");
    }

    #[test]
    fn finds_a_claude_code_session_in_a_terminal_by_its_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let claude = tmp.path().join("claude");
        // Copied by another process: on Linux a file this one held open for
        // writing could not be run (see `common::write_program`).
        let copied = std::process::Command::new("cp")
            .arg("/bin/sleep")
            .arg(&claude)
            .status()
            .unwrap();
        assert!(copied.success());
        // macOS kills a copy of a system program unless it is signed again.
        #[cfg(target_os = "macos")]
        assert!(
            std::process::Command::new("/usr/bin/codesign")
                .args(["-s", "-", "-f"])
                .arg(&claude)
                .output()
                .unwrap()
                .status
                .success()
        );
        let (mut master, mut slave) = (0, 0);
        // SAFETY: openpty fills two descriptors, closed below.
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
        let mut cmd = std::process::Command::new(&claude);
        cmd.arg("30");
        // SAFETY: only async-signal-safe calls between fork and exec: a new
        // session whose controlling terminal is the pseudo-terminal.
        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let me = process::info(std::process::id()).unwrap();
        // Until it has exec'd, the kernel shows this process's name.
        let named = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            process::info(pid).is_some_and(|p| p.name == b"claude" && p.tty)
        });
        let hats = process::Info {
            name: b"hats".to_vec(),
            ..process::info(std::process::id()).unwrap()
        };
        let found = (named, runs_without(pid, &hats), runs_without(pid, &me));
        child.kill().unwrap();
        child.wait().unwrap();
        // SAFETY: the descriptors from openpty.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        // Its parent, this test, is named like the `me` it is checked against.
        assert_eq!(found, (true, true, false));
    }

    #[test]
    fn tells_an_interactive_claude_code_session() {
        let session = |args: &[&str]| is_claude_session(args.join("\0").as_bytes());
        assert!(session(&["claude"]));
        assert!(session(&["/opt/bin/claude", "--model", "opus", "fix it"]));
        assert!(session(&["node", "/usr/local/bin/claude", "-c"]));
        assert!(!session(&["claude", "-p", "hi"]), "print mode");
        assert!(!session(&["claude", "mcp", "serve"]), "a subcommand");
        assert!(!session(&["node", "server.js"]));
        assert!(!session(&["/bin/zsh", "-c", "claude"]));
        assert!(may_be_claude(b"2.1.302") && may_be_claude(b"claude") && may_be_claude(b"node"));
        assert!(!may_be_claude(b"zsh") && !may_be_claude(b"") && !may_be_claude(b"hats"));
    }

    #[test]
    fn the_clock_runs_from_the_last_turn_or_transcript_write() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("s.jsonl");
        fs::write(&transcript, b"").unwrap();
        let hour = Duration::from_secs(3600);
        // Whole milliseconds, as `turn` keeps them.
        let now = UNIX_EPOCH + Duration::from_millis(unix_millis(SystemTime::now()));
        let turn = Turn::new("s".into(), Some(transcript.clone()), now - 3 * hour);
        turn.write(dir.path()).unwrap();
        let left = |after: Duration| time_to_close(dir.path(), after, None, now).map(|(l, _)| l);

        let file = fs::File::options().write(true).open(&transcript).unwrap();
        file.set_modified(now - 5 * hour).unwrap();
        assert_eq!(left(4 * hour), Some(hour), "an older write does not count");
        assert_eq!(left(2 * hour), Some(Duration::ZERO), "due");
        file.set_modified(now - hour).unwrap();
        assert_eq!(left(2 * hour), Some(hour), "a newer write restarts it");
        assert_eq!(left(Duration::MAX), Some(Duration::MAX), "never due");
        schedule(
            dir.path(),
            std::process::id(),
            Some(Duration::MAX),
            None,
            now,
        );
        // SAFETY: reads and clears the alarm just set.
        assert_eq!(unsafe { libc::alarm(0) }, CHECK_SECS as u32);
        let soon = Some(Duration::from_millis(89_500));
        schedule(
            dir.path(),
            std::process::id(),
            Some(Duration::MAX),
            soon,
            now,
        );
        // SAFETY: reads and clears the alarm just set.
        assert_eq!(unsafe { libc::alarm(0) }, 90, "a move held back is sooner");
        schedule(dir.path(), std::process::id(), None, None, now);
        // SAFETY: reads and clears the alarm, which nothing set.
        assert_eq!(unsafe { libc::alarm(0) }, 0, "nothing to wait for");
        fs::remove_file(&transcript).unwrap();
        assert_eq!(left(4 * hour), Some(hour), "no transcript: the turn's end");

        // A background shell is use while it runs, up to `after` of its run.
        let shell = |started: SystemTime, after: Duration| {
            time_to_close(dir.path(), after, Some(started), now).map(|(l, _)| l)
        };
        assert_eq!(shell(now - hour, 4 * hour), Some(4 * hour), "running");
        assert_eq!(shell(now - 5 * hour, 4 * hour), Some(3 * hour), "for 4h");
        assert_eq!(
            shell(now - 9 * hour, 4 * hour),
            Some(hour),
            "the turn is later"
        );
        assert!(in_use_until(now - hour, 2 * hour, now) >= now);
        assert!(in_use_until(now - 3 * hour, 2 * hour, now) < now);

        fs::remove_file(dir.path().join("turn")).unwrap();
        assert_eq!(left(hour), None, "busy");
    }
}
