//! On-disk record of a native change, written before PackageKit is asked to
//! make it and removed once the manifest agrees.
//!
//! One JSON file per operation under `<data>/native/`. A file that survives
//! means the process died, or the answer was lost, between the system change
//! and the manifest commit; whoever opens the manifest next settles it.

use crate::engine::KEEP_VERSIONS;
use crate::error::{Error, IoResultExt, Result};
use crate::lock::WriteLock;
use crate::manifest::{AppRecord, Manifest, VersionRecord};
use crate::model::{ExecKind, FormatKind, SourceCfg, SourceKind};
use crate::packagekit::PackageKit;
use crate::version::compare_package;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
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
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
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

/// Settles the journals in `dir` by asking PackageKit what is installed, and
/// returns how many it settled. A journal whose question PackageKit cannot
/// answer stays for the next run. Holding `_lock` guarantees no live process
/// owns a journal. An install or remove is never retried here.
pub(crate) fn recover_native(
    _lock: &WriteLock,
    dir: &Path,
    manifest: &mut Manifest,
    pk: &dyn PackageKit,
) -> Result<usize> {
    let mut settled = 0;
    for (path, intent) in NativeJournal::pending(dir)? {
        let installed = match pk.resolve_installed(&intent.pk_name) {
            Ok(installed) => installed,
            Err(e) => {
                tracing::warn!("cannot settle {} for {}: {e}", path.display(), intent.app);
                continue;
            }
        };
        match intent.op {
            NativeOp::Install | NativeOp::Update => {
                let landed = installed
                    .filter(|p| compare_package(&p.version, &intent.version) == Ordering::Equal);
                if let (Some(installed), Some(record)) = (landed, &intent.record) {
                    record_landed(manifest, &intent, record, &installed.id)?;
                    tracing::warn!(
                        "recorded {} {} from {}",
                        intent.app,
                        intent.version,
                        path.display()
                    );
                }
            }
            NativeOp::Remove => {
                if installed.is_none() && manifest.get(&intent.app)?.is_some() {
                    manifest.remove(&intent.app)?;
                    tracing::warn!("forgot {} from {}", intent.app, path.display());
                }
            }
        }
        fs::remove_file(&path).at(&path)?;
        settled += 1;
    }
    Ok(settled)
}

/// Commits the rows of an install or update PackageKit confirmed, unless the
/// manifest already shows them.
fn record_landed(
    manifest: &mut Manifest,
    intent: &NativeIntent,
    record: &NativeRecord,
    package_id: &str,
) -> Result<()> {
    if manifest
        .get(&intent.app)?
        .is_some_and(|app| app.current == record.label)
    {
        return Ok(());
    }
    let app = record.app_record(&intent.app)?;
    let version = record.version_record();
    let mut stored: Vec<(&str, &str)> = record
        .options
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    stored.push(("pk.package_id", package_id));
    let pruned: Vec<String> = if intent.op == NativeOp::Update {
        manifest
            .versions(&intent.app)?
            .into_iter()
            .skip(KEEP_VERSIONS - 1)
            .filter(|v| v.label != record.label)
            .map(|v| v.label)
            .collect()
    } else {
        Vec::new()
    };
    let pruned: Vec<&str> = pruned.iter().map(String::as_str).collect();
    manifest.commit_install_with(&app, &version, &[], &stored, &pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::Cancel;
    use crate::lock;
    use crate::manifest::Manifest;
    use crate::packagekit::{PackageKit, PkEvent, PkPackage};

    struct Resolver {
        pkg: Option<PkPackage>,
        fail: bool,
    }

    impl PackageKit for Resolver {
        fn can_install_files(&self) -> bool {
            true
        }

        fn resolve_installed(&self, _name: &str) -> Result<Option<PkPackage>> {
            if self.fail {
                return Err(Error::PackageKitUnavailable("no bus".into()));
            }
            Ok(self.pkg.clone())
        }

        fn install_file(&self, _: &Path, _: &Cancel, _: &mut dyn FnMut(PkEvent)) -> Result<()> {
            unreachable!("recovery never installs")
        }

        fn remove(&self, _: &str, _: &Cancel, _: &mut dyn FnMut(PkEvent)) -> Result<()> {
            unreachable!("recovery never removes")
        }
    }

    fn resolver(id: Option<&str>) -> Resolver {
        Resolver {
            pkg: id.map(|id| PkPackage::parse(id).unwrap()),
            fail: false,
        }
    }

    /// Settles the journals in `dir` against `manifest`.
    fn settle(dir: &Path, manifest: &mut Manifest, pk: &Resolver) -> Result<usize> {
        let lock_dir = tempfile::tempdir().unwrap();
        let lock = lock::acquire(&lock_dir.path().join("lock"), || {}).unwrap();
        recover_native(&lock, dir, manifest, pk)
    }

    fn journaled(dir: &Path, op: NativeOp, version: &str) {
        let mut intent = intent();
        intent.op = op;
        intent.version = version.into();
        if intent.op == NativeOp::Remove {
            intent.record = None;
        }
        NativeJournal::create(dir, &intent).unwrap();
    }

    fn installed(manifest: &Manifest) -> Option<AppRecord> {
        manifest.get("hello").unwrap()
    }

    #[test]
    fn installed_package_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::open_in_memory().unwrap();
        journaled(dir.path(), NativeOp::Install, "1.0");
        let pk = resolver(Some("hello;1.0;amd64;local"));
        assert_eq!(settle(dir.path(), &mut manifest, &pk).unwrap(), 1);
        assert_eq!(installed(&manifest).unwrap().backend, "packagekit");
        assert_eq!(
            manifest
                .option("hello", "pk.package_id")
                .unwrap()
                .as_deref(),
            Some("hello;1.0;amd64;local")
        );
        assert!(NativeJournal::pending(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn epoch_version_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::open_in_memory().unwrap();
        journaled(dir.path(), NativeOp::Install, "2:1.0-3");
        let pk = resolver(Some("hello;2:1.0-3;x86_64;local"));
        assert_eq!(settle(dir.path(), &mut manifest, &pk).unwrap(), 1);
        assert!(installed(&manifest).is_some());
    }

    #[test]
    fn crash_before_install_drops_journal() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::open_in_memory().unwrap();
        journaled(dir.path(), NativeOp::Install, "1.0");
        assert_eq!(
            settle(dir.path(), &mut manifest, &resolver(None)).unwrap(),
            1
        );
        assert!(installed(&manifest).is_none());
        assert!(NativeJournal::pending(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn other_version_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::open_in_memory().unwrap();
        journaled(dir.path(), NativeOp::Install, "1.0");
        let pk = resolver(Some("hello;0.9;amd64;local"));
        assert_eq!(settle(dir.path(), &mut manifest, &pk).unwrap(), 1);
        assert!(installed(&manifest).is_none());
        assert!(NativeJournal::pending(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn interrupted_remove_forgets_removed_package() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::open_in_memory().unwrap();
        let record = intent().record.unwrap();
        manifest
            .commit_install_with(
                &record.app_record("hello").unwrap(),
                &record.version_record(),
                &[],
                &[],
                &[],
            )
            .unwrap();
        journaled(dir.path(), NativeOp::Remove, "");
        assert_eq!(
            settle(dir.path(), &mut manifest, &resolver(None)).unwrap(),
            1
        );
        assert!(installed(&manifest).is_none());
    }

    #[test]
    fn remove_that_did_not_happen_keeps_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::open_in_memory().unwrap();
        let record = intent().record.unwrap();
        manifest
            .commit_install_with(
                &record.app_record("hello").unwrap(),
                &record.version_record(),
                &[],
                &[],
                &[],
            )
            .unwrap();
        journaled(dir.path(), NativeOp::Remove, "");
        let pk = resolver(Some("hello;1.0;amd64;local"));
        assert_eq!(settle(dir.path(), &mut manifest, &pk).unwrap(), 1);
        assert!(installed(&manifest).is_some());
        assert!(NativeJournal::pending(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn unreachable_packagekit_keeps_journal() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = Manifest::open_in_memory().unwrap();
        journaled(dir.path(), NativeOp::Install, "1.0");
        let pk = Resolver {
            pkg: None,
            fail: true,
        };
        assert_eq!(settle(dir.path(), &mut manifest, &pk).unwrap(), 0);
        assert_eq!(NativeJournal::pending(dir.path()).unwrap().len(), 1);
        assert!(installed(&manifest).is_none());
    }

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
