//! Windows IPC implementation using atomic-rename file protocol + LockFileEx.
//!
//! Protocol:
//! - Client writes `debrief.req.tmp`, renames to `debrief.req` (atomic).
//! - Daemon polls for `debrief.req`, reads + deletes it.
//! - Daemon writes `debrief.resp.tmp`, renames to `debrief.resp` (atomic).
//! - Client polls for `debrief.resp`, reads + deletes it.
//! - `LockFileEx` on `debrief.lock` serializes concurrent clients.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

use super::protocol::{DaemonRequest, DaemonResponse, read_message, write_message};

/// Poll interval for file-based IPC (ms).
const POLL_INTERVAL_MS: u64 = 10;

// -- Locking helper -----------------------------------------------------------

/// Acquire an exclusive lock on `file` using `LockFileEx`.
fn lock_exclusive(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    let handle: HANDLE = file.as_raw_handle() as HANDLE;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: LockFileEx is a standard Windows API; handle is valid while File lives.
    let ret = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ret == 0 {
        return Err(std::io::Error::last_os_error()).context("LockFileEx failed");
    }
    Ok(())
}

// -- DaemonIpc (server side) --------------------------------------------------

/// Daemon-side IPC handle. Windows uses file-based polling.
pub struct DaemonIpc {
    daemon_dir: PathBuf,
}

impl DaemonIpc {
    /// Create IPC endpoints (lock file + readiness marker).
    pub fn setup(daemon_dir: &Path) -> Result<Self> {
        let lock_path = daemon_dir.join("debrief.lock");
        let ready_path = daemon_dir.join("debrief.ready");

        File::create(&lock_path).context("failed to create lock file")?;
        File::create(&ready_path).context("failed to create readiness marker")?;

        Ok(Self {
            daemon_dir: daemon_dir.to_path_buf(),
        })
    }

    /// Poll for an incoming client request. Returns `None` on timeout.
    pub fn poll_request(&mut self, timeout_ms: i32) -> Result<Option<DaemonRequest>> {
        let req_path = self.daemon_dir.join("debrief.req");
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);

        loop {
            if req_path.exists() {
                let mut file = File::open(&req_path).context("failed to open request file")?;
                let request: DaemonRequest = read_message(&mut file)?;
                drop(file);
                fs::remove_file(&req_path).ok();
                return Ok(Some(request));
            }

            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    /// Send a response to the client via atomic rename.
    pub fn send_response(&mut self, response: &DaemonResponse) -> Result<()> {
        let tmp_path = self.daemon_dir.join("debrief.resp.tmp");
        let resp_path = self.daemon_dir.join("debrief.resp");

        let mut file = File::create(&tmp_path).context("failed to create response tmp file")?;
        write_message(&mut file, response)?;
        drop(file);
        fs::rename(&tmp_path, &resp_path).context("failed to rename response file")?;
        Ok(())
    }
}

// -- Client-side functions ----------------------------------------------------

/// Send a request to the daemon and return the response.
pub fn send_command(
    daemon_dir: &Path,
    request: DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse> {
    let lock_path = daemon_dir.join("debrief.lock");
    let req_tmp = daemon_dir.join("debrief.req.tmp");
    let req_path = daemon_dir.join("debrief.req");
    let resp_path = daemon_dir.join("debrief.resp");

    // 1. Acquire exclusive lock
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .context("failed to open lock file")?;
    lock_exclusive(&lock_file)?;

    // 2. Drain stale response
    fs::remove_file(&resp_path).ok();

    // 3. Write request to tmp file, then atomic rename
    let mut req_file = File::create(&req_tmp).context("failed to create request tmp file")?;
    write_message(&mut req_file, &request)?;
    drop(req_file);
    fs::rename(&req_tmp, &req_path).context("failed to rename request file")?;

    // 4. Poll for response, looping on Progress keepalives.
    // `timeout` is the per-message deadline — daemon sends Progress every 10s so any
    // value ≥ 30s gives ample margin.
    loop {
        let msg_deadline = Instant::now() + timeout;
        let response = loop {
            if resp_path.exists() {
                let mut file = File::open(&resp_path).context("failed to open response file")?;
                let msg: DaemonResponse = read_message(&mut file)?;
                drop(file);
                fs::remove_file(&resp_path).ok();
                break msg;
            }
            if Instant::now() >= msg_deadline {
                bail!(
                    "timed out waiting for daemon response ({}s per-message limit)",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        };

        match response {
            DaemonResponse::Progress { .. } => {
                // Keepalive received — daemon is still working, continue waiting.
            }
            other => {
                // lock auto-released on lock_file drop
                return Ok(other);
            }
        }
    }
}

/// Remove IPC-specific files.
pub fn cleanup_ipc_files(dir: &Path) {
    for name in [
        "debrief.req",
        "debrief.resp",
        "debrief.lock",
        "debrief.req.tmp",
        "debrief.resp.tmp",
        "debrief.ready",
    ] {
        fs::remove_file(dir.join(name)).ok();
    }
}

/// Path whose existence signals daemon readiness.
pub fn ready_indicator(dir: &Path) -> PathBuf {
    dir.join("debrief.ready")
}

/// Check if a process is alive via `OpenProcess(SYNCHRONIZE)`.
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess with SYNCHRONIZE is a standard Windows API call.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    // SAFETY: handle is valid and non-null.
    unsafe { CloseHandle(handle) };
    true
}

/// Detach daemon from parent's process group.
pub fn configure_daemon_spawn(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Try to acquire an exclusive lock in non-blocking mode (Windows).
/// Uses `LOCKFILE_FAIL_IMMEDIATELY` flag.
pub fn try_flock_exclusive(file: &File) -> Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_FAIL_IMMEDIATELY;

    let handle: HANDLE = file.as_raw_handle() as HANDLE;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ret != 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32) {
        return Ok(false);
    }
    Err(err).context("LockFileEx(FAIL_IMMEDIATELY) failed")
}
