//! On-disk record of a native change, written before PackageKit is asked to
//! make it and removed once the manifest agrees.
//!
//! One JSON file per operation under `<data>/native/`. A file that survives
//! means the process died, or the answer was lost, between the system change
//! and the manifest commit; whoever opens the manifest next settles it.

use crate::error::{Error, IoResultExt, Result};
use crate::manifest::{AppRecord, VersionRecord};
use crate::model::{ExecKind, FormatKind, SourceCfg, SourceKind};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum NativeOp {
    Install,
    Update,
    Remove,
}

/// What a native operation is about to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeIntent {
    pub op: NativeOp,
    /// The app id.
    pub app: String,
    /// The name PackageKit knows the package by.
    pub pk_name: String,
    /// The package version being installed; empty for a removal.
    pub version: String,
    /// What the manifest gets once the change is confirmed; `None` for a removal.
    pub record: Option<NativeRecord>,
}

/// The manifest rows for a native app, in a form that can be stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NativeRecord {
    pub name: String,
    pub format: String,
    pub source_kind: String,
    pub source_location: String,
    pub source_pattern: Option<String>,
    pub label: String,
    pub version: Option<String>,
    pub marker: Option<String>,
    pub entry: PathBuf,
    pub options: Vec<(String, String)>,
}

impl NativeRecord {
    pub(crate) fn app_record(&self, id: &str) -> Result<AppRecord> {
        Ok(AppRecord {
            id: id.to_string(),
            name: self.name.clone(),
            format: self.format.parse::<FormatKind>()?,
            backend: crate::engine::NATIVE_BACKEND.to_string(),
            source: SourceCfg {
                kind: self.source_kind.parse::<SourceKind>()?,
                location: self.source_location.clone(),
                pattern: self.source_pattern.clone(),
            },
            current: self.label.clone(),
            desktop: false,
            command: None,
        })
    }

    pub(crate) fn version_record(&self) -> VersionRecord {
        VersionRecord {
            label: self.label.clone(),
            version: self.version.clone(),
            marker: self.marker.clone(),
            entry: self.entry.clone(),
            exec_kind: ExecKind::Elf,
            installed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
        }
    }
}

pub(crate) struct NativeJournal {
    path: PathBuf,
}

impl NativeJournal {
    /// Writes `<dir>/<pid>-<n>.json` holding the intent, durably.
    pub(crate) fn create(dir: &Path, intent: &NativeIntent) -> Result<NativeJournal> {
        fs::create_dir_all(dir).at(dir)?;
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{}-{n}.json", std::process::id()));
        let text = serde_json::to_string(intent).map_err(|e| Error::Parse(e.to_string()))?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .at(&path)?;
        file.write_all(text.as_bytes()).at(&path)?;
        file.sync_all().at(&path)?;
        File::open(dir).and_then(|d| d.sync_all()).at(dir)?;
        Ok(NativeJournal { path })
    }

    /// Deletes the journal: the operation is settled.
    pub(crate) fn finish(self) -> Result<()> {
        fs::remove_file(&self.path).at(&self.path)
    }

    /// The journals left in `dir`, sorted by file name.
    #[allow(dead_code)] // read by native recovery, which is not wired in yet
    pub(crate) fn pending(dir: &Path) -> Result<Vec<(PathBuf, NativeIntent)>> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).at(dir),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry.at(dir)?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                paths.push(path);
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let bytes = fs::read(&path).at(&path)?;
                let intent = serde_json::from_slice(&bytes).map_err(|e| {
                    Error::Parse(format!("bad native journal {}: {e}", path.display()))
                })?;
                Ok((path, intent))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> NativeIntent {
        NativeIntent {
            op: NativeOp::Install,
            app: "hello".into(),
            pk_name: "hello".into(),
            version: "1.0".into(),
            record: Some(NativeRecord {
                name: "hello".into(),
                format: "deb".into(),
                source_kind: "local-file".into(),
                source_location: "/tmp/hello_1.0.deb".into(),
                source_pattern: None,
                label: "1.0".into(),
                version: Some("1.0".into()),
                marker: None,
                entry: PathBuf::from("/usr/bin/hello"),
                options: vec![("pk.name".into(), "hello".into())],
            }),
        }
    }

    #[test]
    fn intent_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let journal = NativeJournal::create(dir.path(), &intent()).unwrap();
        let pending = NativeJournal::pending(dir.path()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, intent());
        let record = pending[0].1.record.as_ref().unwrap();
        let app = record.app_record("hello").unwrap();
        assert_eq!(app.backend, "packagekit");
        assert!(!app.desktop && app.command.is_none());
        journal.finish().unwrap();
        assert!(NativeJournal::pending(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn pending_lists_sorted_json_only() {
        let dir = tempfile::tempdir().unwrap();
        let text = serde_json::to_string(&intent()).unwrap();
        fs::write(dir.path().join("b.json"), &text).unwrap();
        fs::write(dir.path().join("a.json"), &text).unwrap();
        fs::write(dir.path().join("c.tmp"), "not json").unwrap();
        let names: Vec<_> = NativeJournal::pending(dir.path())
            .unwrap()
            .into_iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.json", "b.json"]);
        assert!(
            NativeJournal::pending(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn corrupt_intent_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.json"), "{nope").unwrap();
        match NativeJournal::pending(dir.path()) {
            Err(Error::Parse(m)) => assert!(m.contains("x.json"), "{m}"),
            other => panic!("{other:?}"),
        }
    }
}
