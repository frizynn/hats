//! Processes read straight from the kernel, without `ps`: `proc_listallpids`,
//! `proc_pidinfo` and `KERN_PROCARGS2` on macOS, `/proc` on Linux. Arguments
//! are cut before the environment that follows them, which is never kept.

use std::time::SystemTime;

/// The start of a command line is enough to tell; longer ones are cut.
#[cfg(target_os = "linux")]
const ARGS_MAX: usize = 4096;
/// `PROC_FLAG_CONTROLT` in `<sys/proc_info.h>`: it has a controlling terminal.
#[cfg(target_os = "macos")]
const CONTROLT: u32 = 0x80;

/// What the kernel says about a process of any user it lets us see.
pub struct Info {
    pub ppid: u32,
    pub uid: u32,
    /// Whether it has a controlling terminal.
    pub tty: bool,
    /// The executable's file name as the kernel keeps it, cut to 15 bytes.
    pub name: Vec<u8>,
}

/// Whether `pid` is a process this user may signal.
pub fn alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks.
    libc::pid_t::try_from(pid).is_ok_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0)
}

#[cfg(target_os = "macos")]
pub fn all() -> Vec<u32> {
    // SAFETY: with no buffer it returns about how many there are; then the
    // kernel writes at most the buffer's size and returns how many it wrote.
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        let mut pids = vec![0 as libc::pid_t; usize::try_from(n).unwrap_or(0) + 64];
        let size = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())).unwrap_or(0);
        let n = libc::proc_listallpids(pids.as_mut_ptr().cast(), size);
        pids.truncate(usize::try_from(n).unwrap_or(0));
        pids.into_iter().map(|p| p as u32).collect()
    }
}

#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the kernel fills the struct when it returns its size.
    unsafe {
        let n = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        );
        (n == size).then(|| info.assume_init())
    }
}

#[cfg(target_os = "macos")]
pub fn info(pid: u32) -> Option<Info> {
    let info = bsd_info(pid)?;
    let name = info
        .pbi_comm
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    Some(Info {
        ppid: info.pbi_ppid,
        uid: info.pbi_uid,
        tty: info.pbi_flags & CONTROLT != 0,
        name,
    })
}

#[cfg(target_os = "macos")]
pub fn children(pid: u32) -> Vec<u32> {
    let mut pids = [0 as libc::pid_t; 256];
    // SAFETY: the kernel writes at most the buffer's size in bytes and
    // returns how many pids it wrote.
    let n = unsafe {
        libc::proc_listchildpids(
            pid as libc::pid_t,
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
pub fn args(pid: u32) -> Option<Vec<u8>> {
    // A buffer smaller than the process's arguments and environment fails
    // or reads the wrong end on some macOS versions (seen on CI runners), so
    // it takes the system's limit, as `ps` does. `calloc` maps it lazily.
    let mut buf = vec![0u8; arg_max()?];
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

/// `KERN_ARGMAX`: the most bytes of arguments and environment a process has.
#[cfg(target_os = "macos")]
fn arg_max() -> Option<usize> {
    let mut max: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: a valid name and an int-sized buffer.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut max as *mut libc::c_int).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then(|| usize::try_from(max).ok()).flatten()
}

#[cfg(target_os = "macos")]
pub fn started(pid: u32) -> Option<SystemTime> {
    let info = bsd_info(pid)?;
    let since = std::time::Duration::new(info.pbi_start_tvsec, info.pbi_start_tvusec as u32 * 1000);
    std::time::UNIX_EPOCH.checked_add(since)
}

#[cfg(target_os = "linux")]
pub fn all() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

/// `/proc/<pid>/stat`: the name, in parentheses and maybe with spaces, then
/// the state, the parent, the group, the session and the terminal.
#[cfg(target_os = "linux")]
pub fn info(pid: u32) -> Option<Info> {
    use std::os::unix::fs::MetadataExt;
    let dir = format!("/proc/{pid}");
    let uid = std::fs::metadata(&dir).ok()?.uid();
    let stat = std::fs::read(format!("{dir}/stat")).ok()?;
    let open = stat.iter().position(|b| *b == b'(')?;
    let close = stat.iter().rposition(|b| *b == b')')?;
    let fields = std::str::from_utf8(stat.get(close + 1..)?).ok()?;
    let mut fields = fields.split_whitespace().skip(1);
    let ppid = fields.next()?.parse().ok()?;
    let tty: i64 = fields.nth(2)?.parse().ok()?;
    Some(Info {
        ppid,
        uid,
        tty: tty != 0,
        name: stat.get(open + 1..close)?.to_vec(),
    })
}

/// Each thread lists the children it started.
#[cfg(target_os = "linux")]
pub fn children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
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
pub fn args(pid: u32) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut args = Vec::with_capacity(ARGS_MAX);
    std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(ARGS_MAX as u64)
        .read_to_end(&mut args)
        .ok()?;
    // Arguments end in a NUL each; the last is dropped, as on macOS.
    if args.last() == Some(&0) {
        args.pop();
    }
    Some(args)
}

/// From the start time in `/proc/<pid>/stat`, in clock ticks since boot, and
/// the time since boot now, both counting suspend.
#[cfg(target_os = "linux")]
pub fn started(pid: u32) -> Option<SystemTime> {
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

    #[test]
    fn reads_a_child_of_this_process() {
        let me = std::process::id();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 5; exit 0", "named"])
            .spawn()
            .unwrap();
        let pid = child.id();
        // Until it has exec'd, the kernel shows the parent's name and arguments.
        let found = (0..100).find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            args(pid).filter(|a| a.ends_with(b"named"))
        });
        assert_eq!(
            found.as_deref(),
            Some(&b"/bin/sh\0-c\0sleep 5; exit 0\0named"[..])
        );
        assert!(all().contains(&pid) && all().contains(&me));
        assert!(children(me).contains(&pid));
        let info = info(pid).unwrap();
        assert_eq!(info.ppid, me);
        // SAFETY: getuid(2) takes no arguments.
        assert_eq!(info.uid, unsafe { libc::getuid() });
        // macOS's `/bin/sh` runs the shell `/private/var/select/sh` names.
        assert!(!info.name.is_empty());
        assert!(started(pid).is_some_and(|t| t <= SystemTime::now()));
        assert!(alive(pid));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!alive(pid), "reaped");
        assert!(info_of_nobody());
    }

    /// A pid no process has: nothing to read.
    fn info_of_nobody() -> bool {
        let unused = u32::try_from(libc::pid_t::MAX).unwrap();
        info(unused).is_none() && args(unused).is_none() && !alive(unused) && !alive(0)
    }
}
