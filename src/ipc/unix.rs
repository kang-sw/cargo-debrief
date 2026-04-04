//! Unix IPC implementation using FIFO pair + flock serialization.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::protocol::{DaemonRequest, DaemonResponse, read_message, write_message};

// -- Low-level primitives -----------------------------------------------------

/// Create a named pipe (FIFO) at `path`. Ignores `EEXIST` (idempotent).
fn create_fifo(path: &Path, mode: libc::mode_t) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path =
        CString::new(path.as_os_str().as_bytes()).context("FIFO path contains null byte")?;
    // SAFETY: mkfifo is a standard POSIX call; c_path is valid and null-terminated.
    let ret = unsafe { libc::mkfifo(c_path.as_ptr(), mode) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err).with_context(|| format!("mkfifo failed: {}", path.display()));
        }
    }
    Ok(())
}

/// Acquire an exclusive advisory lock on `file` with a wall-clock timeout.
/// Retries non-blocking flock every 10ms until acquired or timeout expires.
fn flock_exclusive_timeout(file: &File, timeout: Duration) -> Result<()> {
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: flock is a standard POSIX call; fd is valid while File is alive.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err).context("flock(LOCK_EX|LOCK_NB) failed");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for IPC lock ({}s)", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Call `libc::poll()` with EINTR retry.
fn poll_retry(pfd: &mut libc::pollfd, timeout_ms: libc::c_int) -> Result<libc::c_int> {
    loop {
        // SAFETY: poll on a valid fd with a stack-allocated pollfd.
        let n = unsafe { libc::poll(pfd, 1, timeout_ms) };
        if n >= 0 {
            return Ok(n);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err).context("poll() failed");
        }
    }
}

/// Toggle `O_NONBLOCK` on a file descriptor.
fn set_nonblocking(file: &File, nonblock: bool) -> Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl F_GETFL/F_SETFL are standard POSIX calls on a valid fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 {
            return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFL) failed");
        }
        let new_flags = if nonblock {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        if libc::fcntl(fd, libc::F_SETFL, new_flags) == -1 {
            return Err(std::io::Error::last_os_error()).context("fcntl(F_SETFL) failed");
        }
    }
    Ok(())
}

// -- DaemonIpc (server side) --------------------------------------------------

/// Daemon-side IPC handle. Owns the FIFO file descriptors.
pub struct DaemonIpc {
    req_fd: File,  // debrief.req FIFO, opened O_RDWR + O_NONBLOCK
    resp_fd: File, // debrief.resp FIFO, opened O_RDWR (blocking)
}

impl DaemonIpc {
    /// Create IPC endpoints (FIFOs + lock file).
    /// Cleans up stale endpoint files before creating new ones.
    pub fn setup(daemon_dir: &Path) -> Result<Self> {
        let req_path = daemon_dir.join("debrief.req");
        let resp_path = daemon_dir.join("debrief.resp");
        let lock_path = daemon_dir.join("debrief.lock");

        // Clean stale FIFOs first
        std::fs::remove_file(&req_path).ok();
        std::fs::remove_file(&resp_path).ok();
        create_fifo(&req_path, 0o600).context("failed to create request FIFO")?;
        create_fifo(&resp_path, 0o600).context("failed to create response FIFO")?;
        File::create(&lock_path).context("failed to create lock file")?;

        // Open FIFOs with O_RDWR to prevent open() from blocking and eliminate POLLHUP races.
        let req_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&req_path)
            .context("failed to open request FIFO")?;
        let resp_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&resp_path)
            .context("failed to open response FIFO")?;

        Ok(Self { req_fd, resp_fd })
    }

    /// Poll for an incoming client request. Returns `None` on timeout.
    pub fn poll_request(&mut self, timeout_ms: i32) -> Result<Option<DaemonRequest>> {
        let mut pfd = libc::pollfd {
            fd: self.req_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = poll_retry(&mut pfd, timeout_ms)?;

        if n > 0 && (pfd.revents & libc::POLLIN) != 0 {
            set_nonblocking(&self.req_fd, false)?;
            let request: DaemonRequest = match read_message(&mut &self.req_fd) {
                Ok(req) => req,
                Err(e) => {
                    set_nonblocking(&self.req_fd, true)?;
                    return Err(e).context("failed to read request");
                }
            };
            set_nonblocking(&self.req_fd, true)?;
            Ok(Some(request))
        } else {
            Ok(None)
        }
    }

    /// Send a response to the client.
    pub fn send_response(&mut self, response: &DaemonResponse) -> Result<()> {
        write_message(&mut self.resp_fd, response)
    }
}

// -- Client-side functions ----------------------------------------------------

/// Send a request to the daemon via FIFO and return the response.
/// Uses `flock` on `debrief.lock` to serialize concurrent clients.
pub fn send_command(
    daemon_dir: &Path,
    request: DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse> {
    let lock_path = daemon_dir.join("debrief.lock");
    let req_path = daemon_dir.join("debrief.req");
    let resp_path = daemon_dir.join("debrief.resp");

    // 1. Acquire exclusive lock (with 30s timeout to avoid cascading hangs)
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .context("failed to open lock file")?;
    flock_exclusive_timeout(&lock_file, Duration::from_secs(30))?;

    // 2. Open req FIFO for writing
    let mut req_fd = OpenOptions::new()
        .write(true)
        .open(&req_path)
        .context("failed to open request FIFO")?;

    // 3. Write request
    write_message(&mut req_fd, &request)?;
    drop(req_fd);

    // 4. Open resp FIFO for reading (non-blocking initially for drain + poll)
    let resp_fd = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&resp_path)
        .context("failed to open response FIFO")?;

    // 4a. Drain stale data from resp FIFO (from a previously crashed client)
    let mut drain_buf = [0u8; 4096];
    loop {
        // SAFETY: read on a valid fd with a stack-allocated buffer.
        let n = unsafe {
            libc::read(
                resp_fd.as_raw_fd(),
                drain_buf.as_mut_ptr() as *mut libc::c_void,
                drain_buf.len(),
            )
        };
        if n > 0 {
            continue; // drained some bytes, keep going
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue; // interrupted, retry
            }
        }
        break; // EAGAIN (no data) or EOF
    }

    // 5. Switch to blocking mode and loop reading responses.
    // Progress keepalives reset the per-message deadline; the final response exits the loop.
    // `timeout` is the per-message deadline — the daemon sends Progress every 10s so any
    // value ≥ 30s gives ample margin.
    set_nonblocking(&resp_fd, false)?;
    let mut resp_fd = resp_fd;

    let per_msg_ms: libc::c_int = timeout.as_millis().try_into().unwrap_or(libc::c_int::MAX);

    loop {
        let mut pfd = libc::pollfd {
            fd: resp_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = poll_retry(&mut pfd, per_msg_ms)?;
        if n == 0 {
            bail!(
                "timed out waiting for daemon response ({}s per-message limit)",
                timeout.as_secs()
            );
        }

        let response: DaemonResponse = read_message(&mut resp_fd)?;
        match response {
            DaemonResponse::Progress { .. } => {
                // Keepalive received — daemon is still working, continue waiting.
            }
            other => {
                // flock auto-released on lock_file drop
                return Ok(other);
            }
        }
    }
}

/// Remove IPC-specific files (req, resp, lock).
pub fn cleanup_ipc_files(dir: &Path) {
    for name in ["debrief.req", "debrief.resp", "debrief.lock"] {
        std::fs::remove_file(dir.join(name)).ok();
    }
}

/// Path whose existence signals daemon readiness. Clients poll this.
pub fn ready_indicator(dir: &Path) -> PathBuf {
    dir.join("debrief.req")
}

/// Check if a process is alive via kill(pid, 0).
pub fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(pid, 0) with signal 0 only checks process existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Detach daemon into a new session so it survives parent shell exit.
pub fn configure_daemon_spawn(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid() is async-signal-safe (POSIX). Creates a new session
    // so the daemon is not killed by SIGHUP when the parent terminal closes.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Try to acquire an exclusive flock in non-blocking mode.
/// Returns Ok(Some(file)) if acquired, Ok(None) if would block.
pub fn try_flock_exclusive(file: &File) -> Result<bool> {
    // SAFETY: flock with LOCK_NB is standard POSIX.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(err).context("flock(LOCK_EX|LOCK_NB) failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_fifo_creates_pipe() {
        use std::os::unix::fs::FileTypeExt;
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("test.fifo");
        create_fifo(&fifo, 0o600).unwrap();
        assert!(std::fs::metadata(&fifo).unwrap().file_type().is_fifo());
    }

    #[test]
    fn create_fifo_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("test.fifo");
        create_fifo(&fifo, 0o600).unwrap();
        create_fifo(&fifo, 0o600).unwrap();
    }

    #[test]
    fn flock_exclusive_timeout_acquires_free_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        let f = File::create(&path).unwrap();
        flock_exclusive_timeout(&f, Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn flock_exclusive_timeout_times_out_when_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        let f1 = File::create(&path).unwrap();
        flock_exclusive_timeout(&f1, Duration::from_secs(1)).unwrap();

        let f2 = File::open(&path).unwrap();
        let err = flock_exclusive_timeout(&f2, Duration::from_millis(50)).unwrap_err();
        assert!(
            err.to_string().contains("timed out"),
            "expected timeout error, got: {err}"
        );
    }

    #[test]
    fn set_nonblocking_toggles_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.file");
        let f = File::create(&path).unwrap();

        set_nonblocking(&f, true).unwrap();
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags & libc::O_NONBLOCK, 0);

        set_nonblocking(&f, false).unwrap();
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn try_flock_returns_false_when_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        let f1 = File::create(&path).unwrap();
        flock_exclusive_timeout(&f1, Duration::from_secs(1)).unwrap();

        let f2 = File::open(&path).unwrap();
        assert!(!try_flock_exclusive(&f2).unwrap());
    }

    #[test]
    fn try_flock_returns_true_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        let f = File::create(&path).unwrap();
        assert!(try_flock_exclusive(&f).unwrap());
    }

    #[test]
    fn process_alive_current_process() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn process_alive_bogus_pid() {
        // PID 0 is the kernel; we shouldn't have permission to signal it.
        // PID u32::MAX is almost certainly not a real process.
        assert!(!process_alive(u32::MAX));
    }
}
