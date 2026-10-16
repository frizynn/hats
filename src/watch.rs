//! Wakes the interactive view when a key arrives or hats' state changes
//! (`limits.json`, `current-*`, `switches.jsonl`), without polling: one
//! blocking wait on the terminal and the state directory, with kqueue on
//! macOS and inotify on Linux.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// Why the wait ended; both can be true.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Wake {
    /// The terminal has input, or was resized.
    pub input: bool,
    /// Something in the state directory changed.
    pub changed: bool,
}

pub struct Watch {
    queue: OwnedFd,
    #[cfg(target_os = "linux")]
    input: RawFd,
    #[cfg(target_os = "macos")]
    dir: OwnedFd,
    /// The registry is appended in place, which its directory does not see.
    #[cfg(target_os = "macos")]
    registry: Option<OwnedFd>,
    #[cfg(target_os = "macos")]
    registry_path: PathBuf,
}

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a descriptor just returned by the kernel, owned by nobody else.
    check(fd).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "macos")]
impl Watch {
    /// Watches `state`, which must exist, and the terminal on `input`.
    pub fn new(state: &Path, input: RawFd) -> io::Result<Self> {
        // SAFETY: kqueue(2) takes no arguments.
        let queue = owned(unsafe { libc::kqueue() })?;
        let dir = open_event_only(state)?;
        let mut watch = Self {
            queue,
            dir,
            registry: None,
            registry_path: state.join(crate::registry::FILE_NAME),
        };
        let changes = [
            event(input as usize, libc::EVFILT_READ, 0),
            event(libc::SIGWINCH as usize, libc::EVFILT_SIGNAL, 0),
            // Entries created, removed or renamed: `limits.json` and
            // `current-*` are replaced by rename.
            event(
                watch.dir.as_raw_fd() as usize,
                libc::EVFILT_VNODE,
                libc::NOTE_WRITE,
            ),
        ];
        watch.register(&changes)?;
        watch.watch_registry()?;
        Ok(watch)
    }

    /// Opens the registry again when it was created, replaced or removed.
    fn watch_registry(&mut self) -> io::Result<()> {
        self.registry = open_event_only(&self.registry_path).ok();
        if let Some(fd) = &self.registry {
            let flags =
                libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_DELETE | libc::NOTE_RENAME;
            self.register(&[event(fd.as_raw_fd() as usize, libc::EVFILT_VNODE, flags)])?;
        }
        Ok(())
    }

    fn register(&self, changes: &[libc::kevent]) -> io::Result<()> {
        // SAFETY: `changes` is a valid array of kevents; no events are read back.
        check(unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as libc::c_int,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        })?;
        Ok(())
    }

    /// Blocks until the terminal has input or the state changed.
    pub fn wait(&mut self) -> io::Result<Wake> {
        let mut events = [event(0, 0, 0); 8];
        let n = loop {
            // SAFETY: `events` has room for its length; a null timeout blocks.
            let n = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    std::ptr::null(),
                )
            };
            match check(n) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                n => break n? as usize,
            }
        };
        let events = &events[..n];
        let wake = Wake {
            input: events
                .iter()
                .any(|e| e.filter == libc::EVFILT_READ || e.filter == libc::EVFILT_SIGNAL),
            changed: events.iter().any(|e| e.filter == libc::EVFILT_VNODE),
        };
        if wake.changed {
            // Created or replaced since: watch the new file. Closing the old
            // descriptor drops its registration.
            self.watch_registry()?;
        }
        Ok(wake)
    }
}

#[cfg(target_os = "macos")]
fn event(ident: usize, filter: i16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

/// A descriptor only for events: it does not keep the file's volume busy.
#[cfg(target_os = "macos")]
fn open_event_only(path: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: a NUL-terminated path.
    owned(unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) })
}

#[cfg(target_os = "linux")]
impl Watch {
    /// Watches `state`, which must exist, and the terminal on `input`.
    pub fn new(state: &Path, input: RawFd) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: inotify_init1(2) with flags.
        let queue = owned(unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) })?;
        let path =
            std::ffi::CString::new(state.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // A directory watch also reports writes to the files in it, such as
        // appends to the registry.
        let mask = libc::IN_MODIFY | libc::IN_MOVED_TO | libc::IN_CREATE | libc::IN_DELETE;
        // SAFETY: a valid inotify descriptor and a NUL-terminated path.
        check(unsafe { libc::inotify_add_watch(queue.as_raw_fd(), path.as_ptr(), mask) })?;
        Ok(Self { queue, input })
    }

    /// Blocks until the terminal has input or the state changed.
    pub fn wait(&mut self) -> io::Result<Wake> {
        let mut fds = [
            libc::pollfd {
                fd: self.input,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.queue.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfds; -1 blocks.
        match check(unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) }) {
            // A resize (SIGWINCH) interrupts poll(2); crossterm reports it.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                return Ok(Wake {
                    input: true,
                    changed: false,
                });
            }
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        if fds[1].revents != 0 {
            let mut buf = [0u8; 4096];
            // SAFETY: reads into a local buffer until the queue is empty.
            while unsafe { libc::read(self.queue.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) }
                > 0
            {}
        }
        Ok(Wake {
            input: fds[0].revents != 0,
            changed: fds[1].revents != 0,
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    use crate::registry;

    const CHANGED: Wake = Wake {
        input: false,
        changed: true,
    };

    #[test]
    fn wakes_on_a_replaced_file_and_on_an_append() {
        let dir = tempfile::tempdir().unwrap();
        // A terminal that never types.
        let mut pipe = [0; 2];
        // SAFETY: pipe(2) fills two descriptors.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let mut watch = Watch::new(dir.path(), pipe[0]).unwrap();
        let replace = |name: &str| {
            let tmp = dir.path().join(format!("{name}.tmp"));
            std::fs::write(&tmp, "x").unwrap();
            std::fs::rename(&tmp, dir.path().join(name)).unwrap();
        };
        replace("limits.json");
        assert_eq!(watch.wait().unwrap(), CHANGED);
        // Created now: watched from here on.
        replace(registry::FILE_NAME);
        assert_eq!(watch.wait().unwrap(), CHANGED);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join(registry::FILE_NAME))
            .unwrap()
            .write_all(b"line\n")
            .unwrap();
        assert_eq!(watch.wait().unwrap(), CHANGED);
        // SAFETY: closing the descriptors pipe(2) gave.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
    }
}
