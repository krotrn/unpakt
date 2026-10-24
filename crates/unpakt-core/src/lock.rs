//! The cross-process write lock. Every mutating operation holds it while it
//! changes files; reads never take it.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::error::{IoResultExt, Result};

/// Holds the lock until dropped. The OS also releases it if the process dies.
#[derive(Debug)]
pub struct WriteLock {
    _file: File,
}

/// Takes the exclusive lock on `path`, creating the file if needed. When
/// another process holds it, calls `on_wait` once and then blocks until it is
/// free.
pub fn acquire(path: &Path, on_wait: impl FnOnce()) -> Result<WriteLock> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).at(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .at(path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            on_wait();
            file.lock().at(path)?;
        }
        Err(TryLockError::Error(e)) => return Err(e).at(path),
    }
    Ok(WriteLock { _file: file })
}
