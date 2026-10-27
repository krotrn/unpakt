//! Progress events an operation reports while it runs.

use std::sync::Arc;

/// The stage an operation has reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Another unpakt operation holds the write lock.
    WaitingForLock,
    /// Fetching release metadata.
    Fetching,
    Downloading {
        file: String,
        done: u64,
        total: Option<u64>,
    },
    Inspecting,
    /// The lock is held and files are being changed.
    Installing,
    /// The package manager is waiting for the user to authorize the change.
    WaitingForAuthorization,
    /// The package manager is changing the system.
    SystemChange {
        percent: Option<u8>,
    },
}

/// Receives progress events; may be called from the thread running the operation.
pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;
