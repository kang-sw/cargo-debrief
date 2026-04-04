//! Daemon process: holds ONNX model + indexes in memory, serves IPC requests.
//!
//! The daemon runs a synchronous IPC poll loop on a dedicated blocking thread,
//! bridged to async service dispatch via tokio channels.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};

use crate::ipc;
use crate::ipc::protocol::{DaemonRequest, DaemonResponse};
use crate::service::{DebriefService, InProcessService};

/// Default idle timeout: 3 minutes.
const IDLE_TIMEOUT_SECS: u64 = 180;

/// Compute the daemon directory: `.git/debrief/daemon/` relative to the git root.
pub fn daemon_dir(project_root: &Path) -> Result<PathBuf> {
    let mut current = project_root;
    loop {
        let candidate = current.join(".git");
        if candidate.is_dir() {
            return Ok(candidate.join("debrief").join("daemon"));
        }
        current = current
            .parent()
            .context("not inside a git repository; cannot locate daemon directory")?;
    }
}

/// Read the PID from the daemon PID file, if it exists and parses.
pub fn read_pid(daemon_dir: &Path) -> Option<u32> {
    let pid_path = daemon_dir.join("daemon.pid");
    let content = fs::read_to_string(&pid_path).ok()?;
    content.trim().parse().ok()
}

/// Check if a daemon is running for this workspace.
pub fn is_daemon_running(daemon_dir: &Path) -> bool {
    if let Some(pid) = read_pid(daemon_dir) {
        ipc::process_alive(pid)
    } else {
        false
    }
}

/// Write PID file with advisory lock to prevent races.
/// Returns the lock file handle — caller must keep it alive for daemon lifetime.
fn write_pid_file(daemon_dir: &Path) -> Result<File> {
    let pid_path = daemon_dir.join("daemon.pid");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&pid_path)
        .context("failed to open PID file")?;

    // Try non-blocking lock — if another process holds it, bail.
    if !ipc::try_flock_exclusive(&lock_file)? {
        anyhow::bail!("another daemon instance is already starting (PID file locked)");
    }

    // Write PID (create/truncate a separate handle, lock_file keeps the lock).
    let mut writer = File::create(&pid_path).context("failed to write PID file")?;
    write!(writer, "{}", std::process::id())?;
    writer.flush()?;

    Ok(lock_file)
}

/// Cleanup daemon files on shutdown.
fn cleanup(daemon_dir: &Path) {
    ipc::cleanup_ipc_files(daemon_dir);
    fs::remove_file(daemon_dir.join("daemon.pid")).ok();
    fs::remove_file(daemon_dir.join("daemon.exe_mtime")).ok();
    // Remove directory if empty.
    fs::remove_dir(daemon_dir).ok();
}

/// Handle a single daemon request, dispatching to InProcessService.
async fn handle_request(
    request: DaemonRequest,
    service: &InProcessService,
    project_root: &Path,
    start_time: Instant,
) -> (DaemonResponse, bool) {
    let mut shutdown = false;
    let response = match request {
        DaemonRequest::Stop => {
            shutdown = true;
            DaemonResponse::Ok {
                message: "stopping".to_string(),
            }
        }
        DaemonRequest::Status => DaemonResponse::Status {
            pid: std::process::id(),
            uptime_secs: start_time.elapsed().as_secs(),
        },
        DaemonRequest::Index { ref path } => match service.index(project_root, path).await {
            Ok(result) => DaemonResponse::IndexResult(result),
            Err(e) => DaemonResponse::Error {
                message: format!("{e:#}"),
            },
        },
        DaemonRequest::Search {
            ref query,
            top_k,
            include_deps,
        } => match service
            .search(project_root, query, top_k, include_deps)
            .await
        {
            Ok(results) => DaemonResponse::SearchResults { results },
            Err(e) => DaemonResponse::Error {
                message: format!("{e:#}"),
            },
        },
        DaemonRequest::Overview { ref file } => match service.overview(project_root, file).await {
            Ok(content) => DaemonResponse::Overview { content },
            Err(e) => DaemonResponse::Error {
                message: format!("{e:#}"),
            },
        },
        DaemonRequest::DepOverview { ref crate_name } => {
            match service.dep_overview(project_root, crate_name).await {
                Ok(content) => DaemonResponse::Overview { content },
                Err(e) => DaemonResponse::Error {
                    message: format!("{e:#}"),
                },
            }
        }
        DaemonRequest::SetEmbeddingModel { ref model, global } => match service
            .set_embedding_model(project_root, model, global)
            .await
        {
            Ok(()) => DaemonResponse::Ok {
                message: format!("embedding model set to {model:?}"),
            },
            Err(e) => DaemonResponse::Error {
                message: format!("{e:#}"),
            },
        },
    };
    (response, shutdown)
}

/// A request from the IPC thread to the async service dispatcher.
struct ServiceCall {
    request: DaemonRequest,
    reply: oneshot::Sender<DaemonResponse>,
}

/// Entry point for the daemon process. Called from `__daemon` CLI subcommand.
pub async fn run_daemon(project_root: &Path) -> Result<()> {
    let dir = daemon_dir(project_root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create daemon dir: {}", dir.display()))?;

    let start_time = Instant::now();

    // Write PID with advisory lock.
    let _pid_lock = write_pid_file(&dir)?;

    // Write binary identity marker for debug builds.
    #[cfg(debug_assertions)]
    write_binary_marker(&dir);

    eprintln!(
        "[daemon] PID {} starting for {}",
        std::process::id(),
        project_root.display()
    );

    let service = InProcessService::new();

    // Channel: IPC thread -> async dispatcher
    let (tx, mut rx) = mpsc::channel::<ServiceCall>(4);

    let idle_timeout = Duration::from_secs(
        std::env::var("CARGO_DEBRIEF_DAEMON_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(IDLE_TIMEOUT_SECS),
    );

    // Spawn the synchronous IPC loop on a blocking thread.
    let ipc_dir = dir.clone();
    let ipc_task = tokio::task::spawn_blocking(move || ipc_loop(ipc_dir, tx, idle_timeout));

    // Async service dispatch loop: receive requests, process, send responses.
    let project_root = project_root.to_path_buf();
    while let Some(call) = rx.recv().await {
        let (response, should_stop) =
            handle_request(call.request, &service, &project_root, start_time).await;
        // Send response back to IPC thread (ignore if receiver dropped).
        let _ = call.reply.send(response);
        if should_stop {
            break;
        }
    }

    // Wait for IPC thread to finish.
    match ipc_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("[daemon] IPC loop error: {e}"),
        Err(e) => eprintln!("[daemon] IPC task join error: {e}"),
    }

    // Cleanup
    drop(_pid_lock);
    cleanup(&dir);
    eprintln!("[daemon] shut down cleanly");
    Ok(())
}

/// Synchronous IPC poll loop. Runs on a blocking thread.
fn ipc_loop(
    daemon_dir: PathBuf,
    tx: mpsc::Sender<ServiceCall>,
    idle_timeout: Duration,
) -> Result<()> {
    let mut ipc_handle = ipc::DaemonIpc::setup(&daemon_dir)?;
    eprintln!("[daemon] listening on IPC in {}", daemon_dir.display());

    let mut last_activity = Instant::now();

    loop {
        let poll_result = match ipc_handle.poll_request(100) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[daemon] failed to read request: {e}");
                continue;
            }
        };

        match poll_result {
            Some(request) => {
                last_activity = Instant::now();
                let is_stop = matches!(request, DaemonRequest::Stop);

                let (reply_tx, reply_rx) = oneshot::channel();
                if tx
                    .blocking_send(ServiceCall {
                        request,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    eprintln!("[daemon] service dispatcher closed, shutting down");
                    break;
                }

                match reply_rx.blocking_recv() {
                    Ok(response) => {
                        if let Err(e) = ipc_handle.send_response(&response) {
                            eprintln!("[daemon] failed to write response: {e}");
                        }
                    }
                    Err(_) => {
                        eprintln!("[daemon] response channel closed");
                    }
                }

                if is_stop {
                    break;
                }
            }
            None => {
                if last_activity.elapsed() > idle_timeout {
                    eprintln!("[daemon] idle timeout, shutting down");
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Debug binary guard: compare executable identity to detect stale daemon.
/// Only compiled in debug builds.
#[cfg(debug_assertions)]
pub fn check_binary_identity(daemon_dir: &Path) -> bool {
    let marker_path = daemon_dir.join("daemon.exe_mtime");

    // Use CARGO_DEBRIEF_BIN if set (testing), otherwise current_exe().
    let current_exe = match std::env::var("CARGO_DEBRIEF_BIN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return true, // can't check, assume ok
        },
    };
    let current_mtime = match fs::metadata(&current_exe).and_then(|m| m.modified()) {
        Ok(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string(),
        Err(_) => return true,
    };

    if let Ok(stored) = fs::read_to_string(&marker_path) {
        return stored.trim() == current_mtime;
    }

    true // no marker yet, assume ok
}

/// In release builds, always returns true (no check).
#[cfg(not(debug_assertions))]
pub fn check_binary_identity(_daemon_dir: &Path) -> bool {
    true
}

/// Write binary mtime marker for debug builds.
#[cfg(debug_assertions)]
fn write_binary_marker(daemon_dir: &Path) {
    let marker_path = daemon_dir.join("daemon.exe_mtime");
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(meta) = fs::metadata(&exe) {
            if let Ok(mtime) = meta.modified() {
                let nanos = mtime
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .to_string();
                fs::write(&marker_path, nanos).ok();
            }
        }
    }
}

/// Clean up a stale daemon: dead PID with leftover files (R4).
/// Returns true if stale daemon was cleaned up, false if daemon is alive or no PID.
pub fn cleanup_stale(daemon_dir: &Path) -> bool {
    if let Some(pid) = read_pid(daemon_dir) {
        if !ipc::process_alive(pid) {
            eprintln!("[daemon] cleaning up stale daemon (PID {pid})");
            cleanup(daemon_dir);
            return true;
        }
    }
    false
}

/// Spawn a new daemon process via re-exec of `__daemon`.
/// The caller should hold the PID flock to serialize concurrent spawns.
///
/// Uses `CARGO_DEBRIEF_BIN` env var if set (for testing), otherwise `current_exe()`.
pub fn spawn_daemon(project_root: &Path) -> Result<std::process::Child> {
    let dir = daemon_dir(project_root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create daemon dir: {}", dir.display()))?;

    let exe = match std::env::var("CARGO_DEBRIEF_BIN") {
        Ok(p) => PathBuf::from(p),
        Err(_) => std::env::current_exe().context("failed to get current executable")?,
    };
    let root_str = project_root
        .to_str()
        .context("non-UTF8 project root path")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["__daemon", "--project-root", root_str])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    ipc::configure_daemon_spawn(&mut cmd);

    let child = cmd.spawn().context("failed to spawn daemon process")?;
    Ok(child)
}

/// Wait for the daemon readiness indicator to appear.
/// Prints progress dots to stderr. Returns true if ready, false on timeout.
pub fn wait_for_readiness(daemon_dir: &Path, timeout: Duration) -> bool {
    use std::io::Write as _;
    let ready = ipc::ready_indicator(daemon_dir);
    let start = Instant::now();

    while start.elapsed() < timeout {
        if ready.exists() {
            return true;
        }
        eprint!(".");
        let _ = std::io::stderr().flush();
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Attempt to auto-spawn a daemon and connect to it.
///
/// Flow:
/// 1. Compute daemon dir
/// 2. Check if already running → connect (with binary guard R9)
/// 3. flock PID to serialize spawns (R1)
/// 4. Inside flock: clean stale (R4), re-check, spawn if needed
/// 5. Wait readiness → connect
/// 6. Any failure → return None (caller falls back to InProcess)
pub fn auto_spawn_and_connect(project_root: &Path) -> Option<crate::service::DaemonClient> {
    let dir = daemon_dir(project_root).ok()?;

    // Fast path: daemon already running
    if is_daemon_running(&dir) {
        // R9: debug binary guard
        if !check_binary_identity(&dir) {
            eprintln!("[daemon] binary mismatch, killing stale daemon");
            kill_stale_daemon(&dir);
        } else {
            return crate::service::DaemonClient::connect(project_root);
        }
    }

    // Acquire flock on PID to serialize spawn attempts (R1).
    fs::create_dir_all(&dir).ok()?;
    let pid_path = dir.join("daemon.pid");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&pid_path)
        .ok()?;

    let flock_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ipc::try_flock_exclusive(&lock_file) {
            Ok(true) => break,
            Ok(false) => {
                if Instant::now() >= flock_deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }

    // R4: clean stale daemon inside flock (prevents TOCTOU with concurrent spawners).
    cleanup_stale(&dir);

    // Re-check after acquiring flock — another process may have spawned (R1).
    if is_daemon_running(&dir) {
        drop(lock_file);
        return crate::service::DaemonClient::connect(project_root);
    }

    // Spawn daemon
    use std::io::Write as _;
    eprint!("spawning daemon");
    let _ = std::io::stderr().flush();

    let child = match spawn_daemon(project_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed.\n[daemon] spawn error: {e}");
            return None;
        }
    };

    // Release flock before waiting. A redundant concurrent spawner could also start
    // a daemon here, but the second daemon will self-bail in write_pid_file() when
    // it fails to acquire its own flock on the PID file (I2 accepted limitation).
    drop(lock_file);

    // Wait for readiness
    if wait_for_readiness(&dir, Duration::from_secs(30)) {
        eprintln!("done.");
        let mut child = child;
        if let Ok(Some(status)) = child.try_wait() {
            eprintln!("[daemon] daemon exited during startup: {status}");
            return None;
        }
        crate::service::DaemonClient::connect(project_root)
    } else {
        eprintln!("failed.");
        None
    }
}

/// Kill a stale daemon by PID and clean up its files.
pub fn kill_stale_daemon(daemon_dir: &Path) {
    if let Some(pid) = read_pid(daemon_dir) {
        if ipc::process_alive(pid) {
            eprintln!("[daemon] killing stale daemon (PID {pid})");
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            #[cfg(windows)]
            {
                let _ = ipc::send_command(daemon_dir, DaemonRequest::Stop, Duration::from_secs(2));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    cleanup(daemon_dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_dir_finds_git_root() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = daemon_dir(manifest_dir).unwrap();
        assert!(dir.ends_with("debrief/daemon"));
        assert!(
            dir.parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(".git")
                .is_dir()
                || dir.parent().unwrap().parent().unwrap().ends_with(".git")
        );
    }

    #[test]
    fn read_pid_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pid(dir.path()).is_none());
    }

    #[test]
    fn read_pid_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("daemon.pid"), "12345").unwrap();
        assert_eq!(read_pid(dir.path()), Some(12345));
    }

    #[test]
    fn read_pid_invalid_contents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("daemon.pid"), "not-a-number").unwrap();
        assert!(read_pid(dir.path()).is_none());
    }

    #[test]
    fn is_daemon_running_no_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_daemon_running(dir.path()));
    }

    #[test]
    fn is_daemon_running_dead_pid() {
        let dir = tempfile::tempdir().unwrap();
        // Use an extremely high PID that's almost certainly not running
        fs::write(dir.path().join("daemon.pid"), "4294967295").unwrap();
        assert!(!is_daemon_running(dir.path()));
    }

    #[test]
    fn cleanup_removes_files() {
        let dir = tempfile::tempdir().unwrap();
        let daemon_path = dir.path().join("daemon");
        fs::create_dir_all(&daemon_path).unwrap();
        fs::write(daemon_path.join("daemon.pid"), "123").unwrap();
        fs::write(daemon_path.join("debrief.lock"), "").unwrap();

        cleanup(&daemon_path);

        assert!(!daemon_path.join("daemon.pid").exists());
        assert!(!daemon_path.join("debrief.lock").exists());
    }
}
