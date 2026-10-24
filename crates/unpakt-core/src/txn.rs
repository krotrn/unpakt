use crate::cancel::Cancel;
use crate::error::{IoResultExt, Result};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Sibling path (same directory, so same filesystem) with a unique name.
fn sibling(p: &Path, kind: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(format!(".unpakt-{kind}-{}-{n}-", std::process::id()));
    name.push(p.file_name().unwrap_or_default());
    p.with_file_name(name)
}

fn exists_nofollow(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// Removes a file, symlink (as a link) or directory tree without following links.
fn remove_any(p: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.file_type().is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
}

enum Undo {
    /// A directory this transaction created.
    RemoveDir(PathBuf),
    /// A path created inside this transaction.
    RemoveNew(PathBuf),
    /// `backup` holds what used to live at `orig`.
    Restore { orig: PathBuf, backup: PathBuf },
    /// `from` was renamed to `to`.
    Rename { from: PathBuf, to: PathBuf },
    /// A symlink at `link` replaced one pointing to `target`.
    RestoreLink { link: PathBuf, target: PathBuf },
}

/// A set of filesystem changes that are undone when dropped unless committed.
pub struct Txn {
    log: Vec<Undo>,
    committed: bool,
    cancel: Cancel,
}

impl Default for Txn {
    fn default() -> Self {
        Self::new()
    }
}

impl Txn {
    pub fn new() -> Txn {
        Txn::with_cancel(Cancel::new())
    }

    /// A transaction whose long-running steps stop with `Interrupted` once
    /// `cancel` is set; dropping it then undoes everything.
    pub fn with_cancel(cancel: Cancel) -> Txn {
        Txn {
            log: Vec::new(),
            committed: false,
            cancel,
        }
    }

    /// The cancellation flag that loops working for this transaction poll.
    pub fn cancel(&self) -> &Cancel {
        &self.cancel
    }

    /// Creates `p` and any missing parents; undo removes only the dirs created here.
    pub fn create_dir_all(&mut self, p: &Path) -> Result<()> {
        if exists_nofollow(p) {
            return if p.is_dir() {
                Ok(())
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)).at(p)
            };
        }
        let mut missing = Vec::new();
        let mut cur = Some(p);
        while let Some(c) = cur {
            if c.as_os_str().is_empty() || exists_nofollow(c) {
                break;
            }
            missing.push(c.to_path_buf());
            cur = c.parent();
        }
        for dir in missing.into_iter().rev() {
            match fs::create_dir(&dir) {
                Ok(()) => self.log.push(Undo::RemoveDir(dir)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).at(&dir),
            }
        }
        Ok(())
    }

    /// Creates the single directory `p`, which must not exist yet. Undo
    /// removes it with whatever was put inside.
    pub fn create_dir_new(&mut self, p: &Path) -> Result<()> {
        fs::create_dir(p).at(p)?;
        self.log.push(Undo::RemoveNew(p.to_path_buf()));
        Ok(())
    }

    /// Registers a path that something else created during this transaction.
    pub fn track_new(&mut self, p: &Path) {
        self.log.push(Undo::RemoveNew(p.to_path_buf()));
    }

    /// Atomically writes `bytes` to `p` with permission bits `mode`.
    pub fn write(&mut self, p: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let tmp = sibling(p, "tmp");
        let written = (|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&tmp)?;
            f.write_all(bytes)?;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
            f.sync_all()
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e).at(&tmp);
        }
        let backup = match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_dir() => {
                let _ = fs::remove_file(&tmp);
                return Err(std::io::Error::from(std::io::ErrorKind::IsADirectory)).at(p);
            }
            Ok(_) => {
                let bak = sibling(p, "bak");
                if let Err(e) = fs::hard_link(p, &bak) {
                    let _ = fs::remove_file(&tmp);
                    return Err(e).at(p);
                }
                Some(bak)
            }
            Err(_) => None,
        };
        if let Err(e) = fs::rename(&tmp, p) {
            let _ = fs::remove_file(&tmp);
            if let Some(bak) = &backup {
                let _ = fs::remove_file(bak);
            }
            return Err(e).at(p);
        }
        self.log.push(match backup {
            Some(backup) => Undo::Restore {
                orig: p.to_path_buf(),
                backup,
            },
            None => Undo::RemoveNew(p.to_path_buf()),
        });
        Ok(())
    }

    /// Removes a file or symlink (as a link), never a directory; undo restores it.
    pub fn remove_file(&mut self, p: &Path) -> Result<()> {
        if fs::symlink_metadata(p).at(p)?.file_type().is_dir() {
            return Err(std::io::Error::from(std::io::ErrorKind::IsADirectory)).at(p);
        }
        self.move_aside(p, "bak")
    }

    /// Moves a directory (or symlink, as a link) to a sibling trash path; purged on commit.
    pub fn remove_dir_all(&mut self, p: &Path) -> Result<()> {
        self.move_aside(p, "trash")
    }

    fn move_aside(&mut self, p: &Path, kind: &str) -> Result<()> {
        let dest = sibling(p, kind);
        fs::rename(p, &dest).at(p)?;
        self.log.push(Undo::Restore {
            orig: p.to_path_buf(),
            backup: dest,
        });
        Ok(())
    }

    /// Renames `from` to `to`; `to` must not exist.
    pub fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        if exists_nofollow(to) {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)).at(to);
        }
        fs::rename(from, to).at(from)?;
        self.log.push(Undo::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        Ok(())
    }

    /// Atomically points `link` at `target`, replacing an existing symlink.
    pub fn symlink_swap(&mut self, target: &Path, link: &Path) -> Result<()> {
        let old = match fs::symlink_metadata(link) {
            Ok(m) if m.file_type().is_symlink() => Some(fs::read_link(link).at(link)?),
            Ok(_) => {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)).at(link);
            }
            Err(_) => None,
        };
        replace_symlink(target, link)?;
        self.log.push(match old {
            Some(target) => Undo::RestoreLink {
                link: link.to_path_buf(),
                target,
            },
            None => Undo::RemoveNew(link.to_path_buf()),
        });
        Ok(())
    }

    /// Whether `p` is a backup, trash or temp path this transaction created and still tracks.
    pub fn owns(&self, p: &Path) -> bool {
        self.log.iter().any(|u| match u {
            Undo::Restore { backup, .. } => backup == p,
            _ => false,
        })
    }

    /// Keeps all changes and discards backups.
    pub fn commit(mut self) {
        self.committed = true;
        for entry in std::mem::take(&mut self.log) {
            if let Undo::Restore { backup, .. } = entry
                && let Err(e) = remove_any(&backup)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::error!("cannot purge {}: {e}", backup.display());
            }
        }
    }
}

fn replace_symlink(target: &Path, link: &Path) -> Result<()> {
    let tmp = sibling(link, "tmp");
    symlink(target, &tmp).at(&tmp)?;
    if let Err(e) = fs::rename(&tmp, link) {
        let _ = fs::remove_file(&tmp);
        return Err(e).at(link);
    }
    Ok(())
}

impl Drop for Txn {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        while let Some(entry) = self.log.pop() {
            let result: std::io::Result<()> = match &entry {
                Undo::RemoveDir(p) => fs::remove_dir(p),
                Undo::RemoveNew(p) => remove_any(p),
                Undo::Restore { orig, backup } => fs::rename(backup, orig),
                Undo::Rename { from, to } => fs::rename(to, from),
                Undo::RestoreLink { link, target } => {
                    replace_symlink(target, link).map_err(|e| std::io::Error::other(e.to_string()))
                }
            };
            if let Err(e) = result {
                tracing::error!("rollback step failed: {e}");
            }
        }
    }
}
