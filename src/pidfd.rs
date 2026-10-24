//! A handle on one process that a later process reusing its pid can't be mistaken for (a Linux
//! pidfd). `stop`, `reload` and `restart-all` open one, then confirm who the process is, and
//! signal and wait through it: a pid freed and reused between the check and the signal (a slow
//! restart, a discovery file that went stale) never reaches another process. Where pidfds are
//! missing (an older kernel, no procfs) it falls back to the pid, as before.

use std::io;

pub struct Process {
    pid: u32,
    #[cfg(target_os = "linux")]
    fd: Option<std::os::fd::OwnedFd>,
}

impl Process {
    /// A handle on `pid`. `NotFound` when no such process exists (any more).
    pub fn open(pid: u32) -> io::Result<Self> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(io::ErrorKind::NotFound.into());
        }
        let fd = open_pidfd(pid)?;
        if fd.is_none() && !crate::paths::pid_alive(pid) {
            return Err(io::ErrorKind::NotFound.into());
        }
        #[cfg(target_os = "linux")]
        return Ok(Self { pid, fd });
        #[cfg(not(target_os = "linux"))]
        return Ok(Self { pid });
    }

    /// Has the process exited? Through the handle: a pidfd turns readable when its process ends.
    pub fn exited(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.fd {
            use std::os::fd::AsRawFd;
            let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd, no wait.
            let r = unsafe { libc::poll(&mut poll, 1, 0) };
            return r > 0 && poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        }
        !crate::paths::pid_alive(self.pid)
    }

    /// Send `signal`. A process that has already exited is not an error.
    pub fn signal(&self, signal: i32) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.fd {
            use std::os::fd::AsRawFd;
            // SAFETY: pidfd_send_signal(fd, sig, NULL, 0) on a descriptor this handle owns.
            let r = unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), signal, std::ptr::null::<libc::siginfo_t>(), 0)
            };
            return match r {
                0 => Ok(()),
                _ => match io::Error::last_os_error() {
                    e if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                    e => Err(e),
                },
            };
        }
        // SAFETY: plain kill(2) (the fallback without pidfds).
        if unsafe { libc::kill(self.pid as libc::pid_t, signal) } == 0 {
            return Ok(());
        }
        match io::Error::last_os_error() {
            e if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            e => Err(e),
        }
    }
}

/// A pidfd on `pid` (None: pidfds aren't available here, so the pid is used, as before).
#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> io::Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open(pid, 0) takes no pointers; a non-negative result is a new descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd >= 0 {
        // SAFETY: just returned by the kernel, owned by nothing else.
        return Ok(Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) }));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Err(io::ErrorKind::NotFound.into()),
        // No pidfds here (a kernel before 5.3, a seccomp filter).
        Some(libc::ENOSYS | libc::EPERM | libc::EINVAL) => Ok(None),
        _ => Err(error),
    }
}

#[cfg(not(target_os = "linux"))]
fn open_pidfd(_: u32) -> io::Result<Option<()>> {
    Ok(None)
}

/// Does process `pid` hold `path` open (one of its descriptors names that file)? None without
/// procfs, or when its descriptors can't be read (another user's process).
pub fn holds_open(pid: u32, path: &std::path::Path) -> Option<bool> {
    let path = std::fs::canonicalize(path).ok()?;
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    Some(fds.flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_follows_its_process_not_its_pid() {
        let mut child = std::process::Command::new("/bin/sh").args(["-c", "sleep 30"]).spawn().unwrap();
        let process = Process::open(child.id()).unwrap();
        assert_eq!(process.pid, child.id());
        assert!(!process.exited());
        process.signal(libc::SIGTERM).unwrap();
        child.wait().unwrap();
        assert!(process.exited(), "reaped: the handle says so");
        // Signalling it now reaches nothing, and is no error.
        process.signal(libc::SIGTERM).unwrap();
        assert_eq!(Process::open(child.id()).err().map(|e| e.kind()), Some(io::ErrorKind::NotFound));
    }

    #[test]
    fn holds_open_names_the_files_a_process_has_open() {
        let path = std::env::temp_dir().join(format!("muster-pidfd-{}", crate::paths::random_token()));
        let file = std::fs::File::create(&path).unwrap();
        assert_eq!(holds_open(std::process::id(), &path), Some(true));
        drop(file);
        assert_eq!(holds_open(std::process::id(), &path), Some(false));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(holds_open(std::process::id(), &path), None, "no such file");
    }
}
