//! The background shells of a Claude Code process: commands the agent started
//! with `run_in_background`, or moved there with Ctrl+B, that keep running
//! after its turn ends. No hook says when they start or end, and a move or a
//! close would end them with Claude Code.
//!
//! Claude Code 2.1.302 runs each one as its own child, a shell that sources
//! the session's snapshot first (measured):
//! `/bin/zsh -c "source <config>/shell-snapshots/snapshot-zsh-… && … eval '<command>' …"`.
//! A command in the foreground looks the same, but it never outlives its
//! turn, so between turns every such child is a background one. If Claude
//! Code starts them some other way, none is found and nothing waits for them.
//!
//! Read straight from the kernel, without `ps`: `proc_listchildpids`,
//! `KERN_PROCARGS2` and `proc_pidinfo` on macOS, `/proc` on Linux.

use std::time::SystemTime;

/// What the command line of a background shell starts with, after `-c`.
const SOURCE: &[u8] = b"source ";
const SNAPSHOTS: &[u8] = b"/shell-snapshots/";
/// The start of a command line is enough to tell; longer ones are cut.
const ARGS_MAX: usize = 4096;

/// When the newest background shell of `agent` started, if it has one.
pub fn newest(agent: u32) -> Option<SystemTime> {
    children(agent)
        .into_iter()
        .filter(|&pid| args(pid).is_some_and(|args| is_shell(&args)))
        .filter_map(started)
        .max()
}

/// `<shell> -c "source …/shell-snapshots/…"`, from NUL-separated arguments.
fn is_shell(args: &[u8]) -> bool {
    let mut args = args.split(|b| *b == 0);
    let (Some(_), Some(b"-c"), Some(script)) = (args.next(), args.next(), args.next()) else {
        return false;
    };
    let line = script.split(|b| *b == b'\n').next().unwrap_or_default();
    line.starts_with(SOURCE) && line.windows(SNAPSHOTS.len()).any(|w| w == SNAPSHOTS)
}

#[cfg(target_os = "macos")]
fn children(agent: u32) -> Vec<u32> {
    let mut pids = [0 as libc::pid_t; 256];
    // SAFETY: the kernel writes at most the buffer's size in bytes and
    // returns how many pids it wrote.
    let n = unsafe {
        libc::proc_listchildpids(
            agent as libc::pid_t,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as libc::c_int,
        )
    };
    let n = usize::try_from(n).unwrap_or(0).min(pids.len());
    pids[..n].iter().map(|&p| p as u32).collect()
}

/// The arguments, NUL-separated, from `KERN_PROCARGS2`: the argument count,
/// the executable's path, padding, then the arguments and the environment.
#[cfg(target_os = "macos")]
fn args(pid: u32) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; ARGS_MAX];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    // SAFETY: a valid name and a buffer of `len` bytes; the kernel sets
    // `len` to what it wrote, cut to the buffer.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?);
    let rest = &buf[4..len];
    let path_end = rest.iter().position(|b| *b == 0)?;
    let start = path_end + rest[path_end..].iter().position(|b| *b != 0)?;
    let mut args = rest[start..].to_vec();
    // Keep only the arguments, not the environment after them.
    if let Some(end) = args
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == 0)
        .nth(usize::try_from(argc).ok()?.checked_sub(1)?)
        .map(|(i, _)| i)
    {
        args.truncate(end);
    }
    Some(args)
}

#[cfg(target_os = "macos")]
fn started(pid: u32) -> Option<SystemTime> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the kernel fills the struct when it returns its size.
    let info = unsafe {
        let n = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        );
        (n == size).then(|| info.assume_init())?
    };
    let since = std::time::Duration::new(info.pbi_start_tvsec, info.pbi_start_tvusec as u32 * 1000);
    std::time::UNIX_EPOCH.checked_add(since)
}

/// Each thread lists the children it started.
#[cfg(target_os = "linux")]
fn children(agent: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{agent}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn args(pid: u32) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut args = Vec::with_capacity(ARGS_MAX);
    std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(ARGS_MAX as u64)
        .read_to_end(&mut args)
        .ok()?;
    Some(args)
}

/// From the start time in `/proc/<pid>/stat`, in clock ticks since boot, and
/// the time since boot now, both counting suspend.
#[cfg(target_os = "linux")]
fn started(pid: u32) -> Option<SystemTime> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name, in parentheses, may hold spaces; the start is the 22nd field.
    let ticks: u64 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    // SAFETY: plain queries with a valid out pointer.
    let (hz, boot) = unsafe {
        let mut now = std::mem::MaybeUninit::<libc::timespec>::uninit();
        if libc::clock_gettime(libc::CLOCK_BOOTTIME, now.as_mut_ptr()) != 0 {
            return None;
        }
        (libc::sysconf(libc::_SC_CLK_TCK), now.assume_init())
    };
    let hz = u64::try_from(hz).ok().filter(|hz| *hz > 0)?;
    let up = std::time::Duration::new(boot.tv_sec as u64, boot.tv_nsec as u32);
    let at = std::time::Duration::from_millis(ticks * 1000 / hz);
    SystemTime::now().checked_sub(up.checked_sub(at)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn tells_a_background_shell_by_its_command_line() {
        let shell = b"/bin/zsh\0-c\0source /h/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true && eval 'sleep 9' < /dev/null";
        assert!(is_shell(shell));
        assert!(!is_shell(b"/bin/sh\0-c\0'/usr/local/bin/hats' hook stop"));
        assert!(!is_shell(b"caffeinate\0-i\0-t\x00300"));
        assert!(!is_shell(b"/bin/zsh\0-c\0source ~/.zshrc"));
        assert!(!is_shell(b"node\0server.js\0source /x/shell-snapshots/"));
    }

    #[test]
    fn finds_this_processs_background_shells() {
        let before = SystemTime::now() - Duration::from_secs(2);
        let me = std::process::id();
        assert_eq!(newest(me), None);
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("shell-snapshots/snapshot-sh-1.sh");
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        std::fs::write(&snapshot, b"").unwrap();
        let mut shell = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "source {} 2>/dev/null || true && sleep 5 && pwd -P",
                snapshot.display()
            ))
            .spawn()
            .unwrap();
        let mut other = Command::new("/bin/sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
        // Until it has exec'd, the kernel shows the parent's command line.
        let started = (0..100)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(10));
                newest(me)
            })
            .expect("the shell is found");
        assert!(
            started >= before && started <= SystemTime::now(),
            "{started:?}"
        );
        shell.kill().unwrap();
        shell.wait().unwrap();
        assert_eq!(newest(me), None, "the other child is not one");
        other.kill().unwrap();
        other.wait().unwrap();
    }
}
