//! On-disk record of a transaction's undo steps, written before each step runs.
//!
//! The file is JSON lines: a header line holding the [`Intent`], then one
//! line per [`Undo`] step in the order they were recorded.

use crate::error::{Error, IoResultExt, Result};
use crate::txn::Undo;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// What the manifest must show for the app once the operation has committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expect {
    /// The app is installed at exactly this version.
    Current(String),
    /// The app is not installed.
    Absent,
}

/// The operation a journal belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub app: String,
    pub expect: Expect,
}

pub(crate) struct Journal {
    file: File,
    path: PathBuf,
}

impl Journal {
    /// Creates `<dir>/<pid>-<n>.jsonl` holding the intent header, durably.
    pub(crate) fn create(dir: &Path, intent: &Intent) -> Result<Journal> {
        fs::create_dir_all(dir).at(dir)?;
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{}-{n}.jsonl", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .at(&path)?;
        file.write_all(line(intent)?.as_bytes()).at(&path)?;
        file.sync_all().at(&path)?;
        File::open(dir).and_then(|d| d.sync_all()).at(dir)?;
        Ok(Journal { file, path })
    }

    /// Appends one step and syncs it to disk.
    pub(crate) fn record(&mut self, step: &Undo) -> Result<()> {
        self.file.write_all(line(step)?.as_bytes()).at(&self.path)?;
        self.file.sync_data().at(&self.path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

fn line<T: Serialize>(value: &T) -> Result<String> {
    let mut s = serde_json::to_string(value).map_err(|e| Error::Parse(e.to_string()))?;
    s.push('\n');
    Ok(s)
}

/// Reads a journal. A torn or unparsable last line is ignored: its step was
/// recorded before it ran, so it never ran. Anything else unparsable is an error.
#[allow(dead_code)] // consumed by recovery
pub(crate) fn read(path: &Path) -> Result<(Intent, Vec<Undo>)> {
    let bytes = fs::read(path).at(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let bad = |what: &str, e: serde_json::Error| {
        Error::Parse(format!("{what} in journal {}: {e}", path.display()))
    };
    let header = lines
        .first()
        .ok_or_else(|| Error::Parse(format!("empty journal {}", path.display())))?;
    let intent: Intent = serde_json::from_str(header).map_err(|e| bad("bad header", e))?;
    let mut steps = Vec::new();
    let last = lines.len() - 1;
    for (i, l) in lines.iter().enumerate().skip(1) {
        match serde_json::from_str::<Undo>(l) {
            Ok(step) => steps.push(step),
            Err(_) if i == last => {}
            Err(e) => return Err(bad(&format!("corrupt record on line {}", i + 1), e)),
        }
    }
    Ok((intent, steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps() -> Vec<Undo> {
        vec![
            Undo::RemoveDir("/d".into()),
            Undo::Restore {
                orig: "/a".into(),
                backup: "/.unpakt-bak-1-0-a".into(),
            },
            Undo::RemoveNew("/n".into()),
        ]
    }

    fn intent() -> Intent {
        Intent {
            app: "demo".into(),
            expect: Expect::Current("1.0.0".into()),
        }
    }

    fn written(dir: &Path) -> Journal {
        let mut j = Journal::create(dir, &intent()).unwrap();
        for s in steps() {
            j.record(&s).unwrap();
        }
        j
    }

    #[test]
    fn round_trips_intent_and_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let j = written(&tmp.path().join("journal"));
        let (i, s) = read(j.path()).unwrap();
        assert_eq!(i, intent());
        assert_eq!(s, steps());
    }

    #[test]
    fn torn_last_line_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let j = written(tmp.path());
        let mut f = fs::OpenOptions::new().append(true).open(j.path()).unwrap();
        f.write_all(b"{\"Restore\":{").unwrap();
        let (_, s) = read(j.path()).unwrap();
        assert_eq!(s, steps());
    }

    #[test]
    fn bad_header_is_a_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("j.jsonl");
        fs::write(&p, "garbage\n").unwrap();
        assert!(matches!(read(&p), Err(Error::Parse(_))));
    }

    #[test]
    fn corrupt_middle_line_is_a_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let j = written(tmp.path());
        let text = fs::read_to_string(j.path()).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[2] = "{not json";
        fs::write(j.path(), lines.join("\n") + "\n").unwrap();
        assert!(matches!(read(j.path()), Err(Error::Parse(_))));
    }
}
