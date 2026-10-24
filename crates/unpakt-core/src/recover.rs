//! Finishes or undoes operations a crashed process left half done.
//!
//! Each unfinished operation has a journal in `<data>/journal/`. If the
//! manifest already shows the operation's outcome, it committed and only its
//! backups are left to purge. Otherwise its recorded steps are undone. Anything
//! that cannot be done safely stops recovery and leaves the journal in place.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
use crate::journal::{self, Expect};
use crate::lock::WriteLock;
use crate::manifest::Manifest;
use crate::paths::Layout;
use crate::txn::{Undo, purge, undo_step};

/// How many journals recovery finished and how many it rolled back.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    pub finished: usize,
    pub undone: usize,
}

/// The journal files in `dir`, sorted by name.
pub(crate) fn journals(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// Recovers every journal. Holding `_lock` guarantees no live process owns one.
pub fn recover(_lock: &WriteLock, layout: &Layout, manifest: &Manifest) -> Result<Recovered> {
    let mut out = Recovered::default();
    let dir = layout.data.join("journal");
    let files = journals(&dir).map_err(|e| Error::Io {
        path: dir.clone(),
        source: e,
    })?;
    for path in files {
        let refuse = |reason: String| {
            Error::Refused(format!(
                "cannot recover the unfinished operation recorded in {}: {reason}. \
                 Inspect the file, fix or delete it, then retry.",
                path.display()
            ))
        };
        let (intent, steps) = journal::read(&path).map_err(|e| refuse(e.to_string()))?;
        check_paths(layout, &steps).map_err(refuse)?;

        let committed = match &intent.expect {
            Expect::Current(label) => manifest
                .get(&intent.app)?
                .is_some_and(|a| &a.current == label),
            Expect::Absent => manifest.get(&intent.app)?.is_none(),
        };
        if committed {
            if !purge(&steps) {
                return Err(refuse(
                    "cleaning up after the finished operation failed".into(),
                ));
            }
            out.finished += 1;
        } else {
            for step in steps.iter().rev() {
                undo_step(step).map_err(|e| refuse(format!("undo failed ({step:?}): {e}")))?;
            }
            out.undone += 1;
        }
        fs::remove_file(&path).map_err(|e| refuse(format!("cannot delete the journal: {e}")))?;
        tracing::warn!(
            "recovered {} operation on {} from {}",
            if committed { "finished" } else { "interrupted" },
            intent.app,
            path.display()
        );
    }
    Ok(out)
}

/// Every path must be absolute, free of `..`, and lie in a location unpakt
/// manages. Containment is lexical: links are not followed.
fn check_paths(layout: &Layout, steps: &[Undo]) -> std::result::Result<(), String> {
    let roots = [
        &layout.apps,
        &layout.applications,
        &layout.icons,
        &layout.bin,
    ];
    let check = |p: &Path| -> std::result::Result<(), String> {
        if !p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
            return Err(format!("{} is not a plain absolute path", p.display()));
        }
        if !roots.iter().any(|r| p.starts_with(r)) {
            return Err(format!(
                "{} is outside the locations unpakt manages",
                p.display()
            ));
        }
        Ok(())
    };
    // Steps that replace or delete recursively must name something inside a
    // managed root, never the root itself.
    let below = |p: &Path| -> std::result::Result<(), String> {
        check(p)?;
        if roots.iter().any(|r| p == r.as_path()) {
            return Err(format!(
                "{} is a managed root, not an entry in it",
                p.display()
            ));
        }
        Ok(())
    };
    for step in steps {
        match step {
            Undo::RemoveDir(p) => check(p)?,
            Undo::RemoveNew(p) => below(p)?,
            Undo::Restore { orig, backup } => {
                below(orig)?;
                check(backup)?;
                let named = backup
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(".unpakt-"));
                if !named {
                    return Err(format!("{} is not an unpakt backup", backup.display()));
                }
            }
            Undo::Rename { from, to } => {
                below(from)?;
                below(to)?;
            }
            Undo::RestoreLink { link, .. } => check(link)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Intent, Journal};
    use crate::lock;

    #[test]
    fn crash_after_record_before_step_is_undone_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::under(tmp.path());
        fs::create_dir_all(&layout.bin).unwrap();
        let a = layout.bin.join("a");
        fs::write(&a, b"keep").unwrap();
        let mut j = Journal::create(
            &layout.data.join("journal"),
            &Intent {
                app: "demo".into(),
                expect: Expect::Current("1.0.0".into()),
            },
        )
        .unwrap();
        j.record(&Undo::Restore {
            orig: a.clone(),
            backup: layout.bin.join(".unpakt-bak-1-0-a"),
        })
        .unwrap();

        let lock = lock::acquire(&layout.data.join("lock"), || {}).unwrap();
        let manifest = Manifest::open_in_memory().unwrap();
        let r = recover(&lock, &layout, &manifest).unwrap();
        assert_eq!(
            r,
            Recovered {
                finished: 0,
                undone: 1
            }
        );
        assert_eq!(fs::read(&a).unwrap(), b"keep");
        assert!(!j.path().exists());
    }
}
