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
//! Read straight from the kernel (see `process`).

use std::time::SystemTime;

use crate::process::{args, children, started};

/// What the command line of a background shell starts with, after `-c`.
const SOURCE: &[u8] = b"source ";
const SNAPSHOTS: &[u8] = b"/shell-snapshots/";

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    /// Each child with its arguments and start, for a failure message.
    fn describe(agent: u32) -> String {
        let each = children(agent).into_iter().map(|pid| {
            let args = args(pid).map(|a| String::from_utf8_lossy(&a).replace('\0', " "));
            format!("{pid} {args:?} {:?}", started(pid))
        });
        each.collect::<Vec<_>>().join("; ")
    }

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
            .unwrap_or_else(|| panic!("the shell is found; children: {}", describe(me)));
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
