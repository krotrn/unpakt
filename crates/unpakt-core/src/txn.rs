use crate::cancel::Cancel;
use crate::error::{Error, IoResultExt, Result};
use crate::journal::{Intent, Journal};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Undo {
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
    journal: Option<Journal>,
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
            journal: None,
        }
    }

    /// Like [`Txn::with_cancel`], but every step is written to a journal in
    /// `dir` before it runs, so a crash can be undone or finished later.
    pub fn journaled(cancel: Cancel, dir: &Path, intent: Intent) -> Result<Txn> {
        let mut txn = Txn::with_cancel(cancel);
        txn.journal = Some(Journal::create(dir, &intent)?);
        Ok(txn)
    }

    /// Records `step` in the journal (if any), then in memory. Call before acting.
    fn note(&mut self, step: Undo) -> Result<()> {
        if let Some(j) = &mut self.journal {
            j.record(&step)?;
        }
        self.log.push(step);
        Ok(())
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
            match self.mkdir(&dir, Undo::RemoveDir(dir.clone())) {
                Ok(_) => {}
                Err(Error::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Creates the single directory `p`, which must not exist yet. Undo
    /// removes it with whatever was put inside.
    pub fn create_dir_new(&mut self, p: &Path) -> Result<()> {
        if exists_nofollow(p) {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)).at(p);
        }
        self.mkdir(p, Undo::RemoveNew(p.to_path_buf()))
    }

    /// Records `step`, then creates `dir`. If the directory is not created
    /// (for instance because something else made it first) the step is taken
    /// back out of the log and the journal, so undo never touches it.
    fn mkdir(&mut self, dir: &Path, step: Undo) -> Result<()> {
        self.note(step)?;
        if let Err(e) = fs::create_dir(dir) {
            self.log.pop();
            if let Some(j) = &mut self.journal {
                j.retract_last()?;
            }
            return Err(e).at(dir);
        }
        Ok(())
    }

    /// Registers a path that something else created during this transaction.
    pub fn track_new(&mut self, p: &Path) {
        if let Err(e) = self.note(Undo::RemoveNew(p.to_path_buf())) {
            tracing::error!("cannot journal {}: {e}", p.display());
        }
    }

    /// Atomically writes `bytes` to `p` with permission bits `mode`.
    pub fn write(&mut self, p: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let tmp = sibling(p, "tmp");
        self.note(Undo::RemoveNew(tmp.clone()))?;
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
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_dir() => {
                let _ = fs::remove_file(&tmp);
                return Err(std::io::Error::from(std::io::ErrorKind::IsADirectory)).at(p);
            }
            Ok(_) => {
                let bak = sibling(p, "bak");
                self.note(Undo::Restore {
                    orig: p.to_path_buf(),
                    backup: bak.clone(),
                })?;
                if let Err(e) = fs::hard_link(p, &bak) {
                    let _ = fs::remove_file(&tmp);
                    return Err(e).at(p);
                }
            }
            Err(_) => self.note(Undo::RemoveNew(p.to_path_buf()))?,
        }
        if let Err(e) = fs::rename(&tmp, p) {
            let _ = fs::remove_file(&tmp);
            return Err(e).at(p);
        }
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
        self.note(Undo::Restore {
            orig: p.to_path_buf(),
            backup: dest.clone(),
        })?;
        fs::rename(p, &dest).at(p)
    }

    /// Renames `from` to `to`; `to` must not exist.
    pub fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        if exists_nofollow(to) {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)).at(to);
        }
        self.note(Undo::Rename {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        })?;
        fs::rename(from, to).at(from)
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
        let tmp = sibling(link, "tmp");
        self.note(match old {
            Some(target) => Undo::RestoreLink {
                link: link.to_path_buf(),
                target,
            },
            None => Undo::RemoveNew(link.to_path_buf()),
        })?;
        self.note(Undo::RemoveNew(tmp.clone()))?;
        replace_symlink(target, link, &tmp)
    }

    /// Whether `p` is a backup, trash or temp path this transaction created and still tracks.
    pub fn owns(&self, p: &Path) -> bool {
        self.log.iter().any(|u| match u {
            Undo::Restore { backup, .. } => backup == p,
            _ => false,
        })
    }

    /// Keeps all changes and discards backups. The journal goes away only if
    /// every backup was purged; otherwise it stays so recovery can finish.
    pub fn commit(mut self) {
        self.committed = true;
        let steps = std::mem::take(&mut self.log);
        if purge(&steps)
            && let Some(j) = self.journal.take()
        {
            remove_journal(j.path());
        }
    }
}

/// Deletes the backups and trash a committed transaction no longer needs.
/// Returns whether every one of them is gone.
pub(crate) fn purge(steps: &[Undo]) -> bool {
    let mut all_ok = true;
    for entry in steps {
        if let Undo::Restore { backup, .. } = entry
            && let Err(e) = remove_any(backup)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!("cannot purge {}: {e}", backup.display());
            all_ok = false;
        }
    }
    all_ok
}

fn remove_journal(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::error!("cannot remove journal {}: {e}", path.display());
    }
}

/// Undoes one step. A step whose effect is not there (it may never have run
/// before a crash) counts as undone.
pub(crate) fn undo_step(step: &Undo) -> std::io::Result<()> {
    let result = match step {
        Undo::RemoveDir(p) => fs::remove_dir(p),
        Undo::RemoveNew(p) => remove_any(p),
        Undo::Restore { orig, backup } => {
            match (fs::symlink_metadata(backup), fs::symlink_metadata(orig)) {
                // A backup hard-linked to `orig` whose replacing rename never ran:
                // renaming would be a no-op, so just drop the extra link.
                (Ok(b), Ok(o)) if b.dev() == o.dev() && b.ino() == o.ino() => {
                    fs::remove_file(backup)
                }
                _ => fs::rename(backup, orig),
            }
        }
        Undo::Rename { from, to } => {
            if exists_nofollow(to) {
                fs::rename(to, from)
            } else {
                Ok(())
            }
        }
        Undo::RestoreLink { link, target } => replace_symlink(target, link, &sibling(link, "tmp"))
            .map_err(|e| std::io::Error::other(e.to_string())),
    };
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn replace_symlink(target: &Path, link: &Path, tmp: &Path) -> Result<()> {
    symlink(target, tmp).at(tmp)?;
    if let Err(e) = fs::rename(tmp, link) {
        let _ = fs::remove_file(tmp);
        return Err(e).at(link);
    }
    Ok(())
}

impl Drop for Txn {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut all_ok = true;
        while let Some(entry) = self.log.pop() {
            if let Err(e) = undo_step(&entry) {
                tracing::error!("rollback step failed: {e}");
                all_ok = false;
            }
        }
        if all_ok && let Some(j) = self.journal.take() {
            remove_journal(j.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_restore_drops_backup_linked_to_orig() {
        let tmp = tempfile::tempdir().unwrap();
        let orig = tmp.path().join("orig");
        let bak = tmp.path().join(".unpakt-bak-1-0-orig");
        fs::write(&orig, b"x").unwrap();
        fs::hard_link(&orig, &bak).unwrap();
        undo_step(&Undo::Restore {
            orig: orig.clone(),
            backup: bak.clone(),
        })
        .unwrap();
        assert!(!bak.exists());
        assert_eq!(fs::read(&orig).unwrap(), b"x");
    }

    #[test]
    fn mkdir_that_loses_a_race_is_not_undone() {
        let tmp = tempfile::tempdir().unwrap();
        let jdir = tmp.path().join("journal");
        let d = tmp.path().join("d");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("keep"), b"k").unwrap();
        let intent = Intent {
            app: "demo".into(),
            expect: crate::journal::Expect::Absent,
        };
        let mut txn = Txn::journaled(Cancel::new(), &jdir, intent).unwrap();
        txn.mkdir(&d, Undo::RemoveNew(d.clone())).unwrap_err();
        let jpath = fs::read_dir(&jdir).unwrap().next().unwrap().unwrap().path();
        let (_, steps) = crate::journal::read(&jpath).unwrap();
        assert!(steps.is_empty());
        drop(txn);
        assert_eq!(fs::read(d.join("keep")).unwrap(), b"k");
    }

    #[test]
    fn undo_tolerates_steps_that_never_ran() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let orig = tmp.path().join("orig");
        fs::write(&orig, b"x").unwrap();
        undo_step(&Undo::RemoveNew(missing.clone())).unwrap();
        undo_step(&Undo::RemoveDir(missing.clone())).unwrap();
        undo_step(&Undo::Restore {
            orig: orig.clone(),
            backup: missing.clone(),
        })
        .unwrap();
        undo_step(&Undo::Rename {
            from: orig.clone(),
            to: missing,
        })
        .unwrap();
        assert_eq!(fs::read(&orig).unwrap(), b"x");
    }
}
