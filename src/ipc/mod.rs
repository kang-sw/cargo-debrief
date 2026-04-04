//! Platform-abstracted IPC for client <-> daemon communication.
//!
//! Unix: FIFO pair + `flock` serialization.
//! Windows: Atomic-rename file protocol + `LockFileEx` serialization.

pub mod protocol;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{
    DaemonIpc, cleanup_ipc_files, configure_daemon_spawn, process_alive, ready_indicator,
    send_command, try_flock_exclusive,
};
#[cfg(windows)]
pub use windows::{
    DaemonIpc, cleanup_ipc_files, configure_daemon_spawn, process_alive, ready_indicator,
    send_command, try_flock_exclusive,
};
