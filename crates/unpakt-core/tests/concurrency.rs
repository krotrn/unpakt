mod common;

use std::ffi::OsString;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Comp, E, MockForge, build_appimage, build_tarball, tree_snapshot};
use unpakt_core::Txn;
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::journal::{Expect, Intent};
use unpakt_core::lock;
use unpakt_core::manifest::Manifest;
use unpakt_core::progress::{Progress, ProgressFn};
use unpakt_core::recover::Recovered;
use unpakt_core::{Error, Layout};

const ELF: &[u8] = b"\x7fELF-fixture";
const DESKTOP: &[u8] = b"[Desktop Entry]\nType=Application\nName=Unpakt Fixture Con\nExec=AppRun %U\nIcon=unpakt-fixture-con\nCategories=Utility;\n";
const REPO: &str = "https://github.com/o/r";

fn engine(root: &Path) -> Engine {
    let layout = Layout::under(root);
    let bin = OsString::from(layout.bin.clone());
    Engine::new(
        layout,
        Host {
            fuse2: true,
            ..Host::detect()
        },
    )
    .unwrap()
    .with_path_var(bin)
}

fn install_local(engine: &mut Engine, dir: &Path) -> String {
    let tarball = build_tarball(
        dir,
        "unpakt-fixture-con-1.0.tar.gz",
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-con/bin/unpakt-fixture-con",
            ELF,
            0o755,
        )],
    );
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    engine.install(plan).unwrap().id
}

fn recorder() -> (ProgressFn, Arc<Mutex<Vec<Progress>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let f: ProgressFn = Arc::new(move |p| sink.lock().unwrap().push(p));
    (f, events)
}

#[test]
fn mutation_waits_for_the_lock_and_says_so() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut first = engine(root.path());
    let id = install_local(&mut first, src.path());
    let layout = first.layout().clone();

    let held = lock::acquire(&layout.data.join("lock"), || {}).unwrap();
    let (rec, events) = recorder();
    let mut second = engine(root.path()).with_progress(rec);
    let handle = {
        let id = id.clone();
        std::thread::spawn(move || {
            let result = second.remove(&id);
            (second, result)
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(*events.lock().unwrap(), vec![Progress::WaitingForLock]);
    assert_eq!(first.list().unwrap().len(), 1);

    drop(held);
    let (second, result) = handle.join().unwrap();
    result.unwrap();
    assert!(second.list().unwrap().is_empty());
    assert_eq!(
        *events.lock().unwrap(),
        vec![Progress::WaitingForLock, Progress::Installing]
    );
}

fn publish(forge: &MockForge, tmp: &Path, version: &str) {
    let name = format!("Unpakt-Fixture-Con-{version}.AppImage");
    let elf = format!("\x7fELF-fixture-{version}");
    let path = build_appimage(
        tmp,
        &name,
        &[
            E::File("AppRun", elf.as_bytes(), 0o755),
            E::File("unpakt-fixture-con.desktop", DESKTOP, 0o644),
        ],
    );
    forge.publish(version, vec![(name, std::fs::read(path).unwrap())]);
}

#[test]
fn update_conflicts_when_the_app_changed_while_waiting() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let make = |progress: Option<ProgressFn>| {
        let engine = engine(root.path()).with_http(Http::new(&forge.api_url(), None).unwrap());
        match progress {
            Some(f) => engine.with_progress(f),
            None => engine,
        }
    };

    let mut setup = make(None);
    publish(&forge, src.path(), "1.0.0");
    let plan = setup.inspect(REPO, AddOpts::default()).unwrap();
    let id = setup.install(plan).unwrap().id;
    publish(&forge, src.path(), "2.0.0");
    assert_eq!(setup.update(&id).unwrap(), Some("2.0.0".into()));
    publish(&forge, src.path(), "3.0.0");
    let layout = setup.layout().clone();
    let (_, _, files) = setup.get(&id).unwrap();

    let (tx, rx) = mpsc::channel();
    let f: ProgressFn = Arc::new(move |p| {
        if p == Progress::WaitingForLock {
            let _ = tx.send(());
        }
    });
    let mut engine_a = make(Some(f));
    let held = lock::acquire(&layout.data.join("lock"), || {}).unwrap();
    let handle = {
        let id = id.clone();
        std::thread::spawn(move || engine_a.update(&id))
    };
    rx.recv_timeout(Duration::from_secs(20)).unwrap();
    Manifest::open(&layout.db)
        .unwrap()
        .set_current(&id, "1.0.0", &files)
        .unwrap();
    drop(held);

    let result = handle.join().unwrap();
    assert!(
        matches!(&result, Err(Error::Conflict(m)) if m.contains("changed by another unpakt process")),
        "{result:?}"
    );
    let dirs: Vec<String> = std::fs::read_dir(layout.apps.join(&id))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!dirs.iter().any(|d| d == "3.0.0"), "{dirs:?}");
    assert!(!dirs.iter().any(|d| d.starts_with(".staging-")), "{dirs:?}");
    assert_eq!(setup.get(&id).unwrap().0.current, "1.0.0");
}

#[test]
fn reads_never_wait_for_the_lock() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let id = install_local(&mut engine, src.path());
    let _held = lock::acquire(&engine.layout().data.join("lock"), || {}).unwrap();

    let start = Instant::now();
    assert_eq!(engine.list().unwrap().len(), 1);
    engine.get(&id).unwrap();
    engine.check_updates(None).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn update_keeps_versions_recorded_while_it_waited() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let make = |progress: Option<ProgressFn>| {
        let engine = engine(root.path()).with_http(Http::new(&forge.api_url(), None).unwrap());
        match progress {
            Some(f) => engine.with_progress(f),
            None => engine,
        }
    };

    let mut setup = make(None);
    publish(&forge, src.path(), "1.0.0");
    let plan = setup.inspect(REPO, AddOpts::default()).unwrap();
    let id = setup.install(plan).unwrap().id;
    publish(&forge, src.path(), "3.0.0");
    let layout = setup.layout().clone();
    let (app, versions, files) = setup.get(&id).unwrap();

    let (tx, rx) = mpsc::channel();
    let f: ProgressFn = Arc::new(move |p| {
        if p == Progress::WaitingForLock {
            let _ = tx.send(());
        }
    });
    let mut engine_a = make(Some(f));
    let held = lock::acquire(&layout.data.join("lock"), || {}).unwrap();
    let handle = {
        let id = id.clone();
        std::thread::spawn(move || engine_a.update(&id))
    };
    rx.recv_timeout(Duration::from_secs(20)).unwrap();

    // Another process installs 2.0 and rolls back to 1.0: `current` is
    // unchanged but a version the waiting update never saw is now recorded.
    let v2_dir = layout.apps.join(&id).join("2.0.0");
    std::fs::create_dir(&v2_dir).unwrap();
    std::fs::write(v2_dir.join("marker"), b"keep").unwrap();
    let mut manifest = Manifest::open(&layout.db).unwrap();
    let v2 = unpakt_core::manifest::VersionRecord {
        label: "2.0.0".into(),
        version: Some("2.0.0".into()),
        ..versions[0].clone()
    };
    let as_v2 = unpakt_core::manifest::AppRecord {
        current: "2.0.0".into(),
        ..app.clone()
    };
    manifest
        .commit_install_with(&as_v2, &v2, &files, &[], &[])
        .unwrap();
    manifest.set_current(&id, "1.0.0", &files).unwrap();
    drop(manifest);
    drop(held);

    assert_eq!(handle.join().unwrap().unwrap(), Some("3.0.0".into()));
    assert!(
        v2_dir.join("marker").is_file(),
        "recorded 2.0.0 was deleted"
    );
    let (_, after, _) = setup.get(&id).unwrap();
    let labels: Vec<&str> = after.iter().map(|v| v.label.as_str()).collect();
    assert!(
        labels.contains(&"2.0.0") && labels.contains(&"3.0.0"),
        "{labels:?}"
    );
}

fn journal_dir(layout: &Layout) -> std::path::PathBuf {
    layout.data.join("journal")
}

fn journal_files(layout: &Layout) -> Vec<std::path::PathBuf> {
    match std::fs::read_dir(journal_dir(layout)) {
        Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// A journaled transaction for `id` that moves its app directory aside and is
/// then abandoned without committing or undoing, as a killed process would.
fn crash_removing(layout: &Layout, id: &str) {
    let mut txn = Txn::journaled(
        Cancel::new(),
        &journal_dir(layout),
        Intent {
            app: id.into(),
            expect: Expect::Absent,
        },
    )
    .unwrap();
    txn.remove_dir_all(&layout.apps.join(id)).unwrap();
    std::mem::forget(txn);
}

#[test]
fn uncommitted_journal_is_undone() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let id = install_local(&mut engine, src.path());
    let layout = engine.layout().clone();
    let before = tree_snapshot(root.path());

    crash_removing(&layout, &id);
    let r = engine.recover().unwrap();
    assert_eq!(
        r,
        Recovered {
            finished: 0,
            undone: 1
        }
    );
    assert_eq!(tree_snapshot(root.path()), before);
    assert_eq!(engine.list().unwrap().len(), 1);
    assert!(journal_files(&layout).is_empty());
}

#[test]
fn committed_journal_is_finished() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let id = install_local(&mut engine, src.path());
    let layout = engine.layout().clone();

    crash_removing(&layout, &id);
    Manifest::open(&layout.db).unwrap().remove(&id).unwrap();
    let r = engine.recover().unwrap();
    assert_eq!(
        r,
        Recovered {
            finished: 1,
            undone: 0
        }
    );
    assert!(!layout.apps.join(&id).exists());
    let leftovers: Vec<_> = std::fs::read_dir(&layout.apps)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(journal_files(&layout).is_empty());
}

#[test]
fn next_mutation_recovers_first() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let a = install_local(&mut engine, src.path());
    let layout = engine.layout().clone();
    let other = tempfile::tempdir().unwrap();
    let tarball = build_tarball(
        other.path(),
        "unpakt-fixture-two-1.0.tar.gz",
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-two/bin/unpakt-fixture-two",
            ELF,
            0o755,
        )],
    );
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    let b = engine.install(plan).unwrap().id;

    crash_removing(&layout, &a);
    assert!(!layout.apps.join(&a).exists());
    engine.remove(&b).unwrap();
    assert!(layout.apps.join(&a).join("current").exists());
    assert!(journal_files(&layout).is_empty());
}

#[test]
fn bad_journal_blocks_mutations() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let id = install_local(&mut engine, src.path());
    let layout = engine.layout().clone();
    std::fs::create_dir_all(journal_dir(&layout)).unwrap();
    let bad = journal_dir(&layout).join("1-1.jsonl");
    std::fs::write(&bad, "not json\n").unwrap();

    let err = engine.remove(&id).unwrap_err();
    assert!(
        matches!(&err, Error::Refused(m) if m.contains("cannot recover the unfinished operation")),
        "{err:?}"
    );
    assert_eq!(engine.list().unwrap().len(), 1);
    assert_eq!(std::fs::read_to_string(&bad).unwrap(), "not json\n");
    let checks = engine.doctor();
    let check = checks.iter().find(|c| c.name == "journal").unwrap();
    assert!(!check.ok, "{check:?}");

    std::fs::remove_file(&bad).unwrap();
    engine.remove(&id).unwrap();
    assert!(engine.doctor().iter().any(|c| c.name == "journal" && c.ok));
}

fn write_journal(layout: &Layout, name: &str, lines: &[String]) -> std::path::PathBuf {
    std::fs::create_dir_all(journal_dir(layout)).unwrap();
    let path = journal_dir(layout).join(name);
    let header = r#"{"app":"x","expect":"Absent"}"#;
    let mut text = String::from(header);
    text.push('\n');
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn corrupt_middle_record_blocks() {
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let layout = engine.layout().clone();
    let victim = layout.apps.join("victim");
    std::fs::create_dir(&victim).unwrap();
    let valid = format!(r#"{{"RemoveNew":"{}"}}"#, victim.display());
    let path = write_journal(&layout, "1-1.jsonl", &["garbage".into(), valid]);

    let err = engine.recover().unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(victim.exists());
    assert!(path.exists());
}

#[test]
fn path_outside_managed_dirs_blocks() {
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let layout = engine.layout().clone();
    let elsewhere = tempfile::tempdir().unwrap();
    let victim = elsewhere.path().join("victim");
    std::fs::write(&victim, b"x").unwrap();
    let path = write_journal(
        &layout,
        "1-1.jsonl",
        &[format!(r#"{{"RemoveNew":"{}"}}"#, victim.display())],
    );
    let err = engine.recover().unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(victim.exists());
    assert!(path.exists());

    let escape = layout.apps.join("..").join("..").join("victim2");
    std::fs::write(layout.apps.join("../../victim2"), b"x").unwrap();
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            r#"{"app":"x","expect":"Absent"}"#,
            format_args!(r#"{{"RemoveNew":"{}"}}"#, escape.display())
        ),
    )
    .unwrap();
    let err = engine.recover().unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(layout.apps.join("../../victim2").exists());

    let sibling = layout.apps.join("victim3");
    std::fs::write(&sibling, b"x").unwrap();
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            r#"{"app":"x","expect":"Absent"}"#,
            format_args!(
                r#"{{"Restore":{{"orig":"{}","backup":"{}"}}}}"#,
                layout.apps.join("a").display(),
                sibling.display()
            )
        ),
    )
    .unwrap();
    let err = engine.recover().unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert!(sibling.exists(), "a backup without the unpakt prefix");
}

fn set_mode(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn failed_undo_keeps_the_journal_for_retry() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let id = install_local(&mut engine, src.path());
    let layout = engine.layout().clone();

    crash_removing(&layout, &id);
    set_mode(&layout.apps, 0o555);
    let err = engine.recover().unwrap_err();
    set_mode(&layout.apps, 0o755);
    assert!(matches!(err, Error::Refused(_)), "{err:?}");
    assert_eq!(journal_files(&layout).len(), 1);

    let r = engine.recover().unwrap();
    assert_eq!(
        r,
        Recovered {
            finished: 0,
            undone: 1
        }
    );
    assert!(layout.apps.join(&id).join("current").exists());
    assert!(journal_files(&layout).is_empty());
}

#[test]
fn crash_after_step_before_manifest_commit_is_undone() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let id = install_local(&mut engine, src.path());
    let layout = engine.layout().clone();
    let file = layout.bin.join(&id);
    assert!(file.exists());
    let before = tree_snapshot(root.path());

    let mut txn = Txn::journaled(
        Cancel::new(),
        &journal_dir(&layout),
        Intent {
            app: id.clone(),
            expect: Expect::Current("9.9.9".into()),
        },
    )
    .unwrap();
    txn.write(&layout.applications.join("x.desktop"), b"new", 0o644)
        .unwrap();
    txn.write(&file, b"#!/bin/sh\n", 0o755).unwrap();
    std::mem::forget(txn);

    let r = engine.recover().unwrap();
    assert_eq!(r.undone, 1);
    assert_eq!(tree_snapshot(root.path()), before);
}

#[test]
fn crash_during_backup_purge_is_finished() {
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let layout = engine.layout().clone();
    let file = layout.bin.join("tool");
    std::fs::write(&file, b"old").unwrap();

    let mut txn = Txn::journaled(
        Cancel::new(),
        &journal_dir(&layout),
        Intent {
            app: "ghost".into(),
            expect: Expect::Absent,
        },
    )
    .unwrap();
    txn.write(&file, b"new", 0o755).unwrap();
    set_mode(&layout.bin, 0o555);
    txn.commit();
    set_mode(&layout.bin, 0o755);
    assert_eq!(journal_files(&layout).len(), 1);

    let r = engine.recover().unwrap();
    assert_eq!(
        r,
        Recovered {
            finished: 1,
            undone: 0
        }
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"new");
    let names: Vec<_> = std::fs::read_dir(&layout.bin)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["tool".to_string()]);
    assert!(journal_files(&layout).is_empty());
}

#[test]
fn interrupted_install_leaves_no_journal() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let cancel = Cancel::new();
    let mut engine = engine(root.path()).with_cancel(cancel.clone());
    let tarball = build_tarball(
        src.path(),
        "unpakt-fixture-con-1.0.tar.gz",
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-con/bin/unpakt-fixture-con",
            ELF,
            0o755,
        )],
    );
    let plan = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap();
    cancel.cancel();
    let err = engine.install(plan).unwrap_err();
    assert!(matches!(err, Error::Interrupted), "{err:?}");
    assert!(journal_files(engine.layout()).is_empty());
}

#[test]
fn journal_naming_a_managed_root_is_refused_and_kept() {
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let layout = engine.layout().clone();
    let app = layout.apps.join("keep");
    std::fs::create_dir_all(&app).unwrap();
    let remove_root = format!(r#"{{"RemoveNew":"{}"}}"#, layout.apps.display());
    let rename_root = format!(
        r#"{{"Rename":{{"from":"{}","to":"{}"}}}}"#,
        layout.apps.display(),
        layout.apps.join("moved").display()
    );
    let restore_root = format!(
        r#"{{"Restore":{{"orig":"{}","backup":"{}"}}}}"#,
        layout.bin.display(),
        layout.bin.join(".unpakt-bak-1-0-x").display()
    );
    for record in [remove_root, rename_root, restore_root] {
        let path = write_journal(&layout, "1-1.jsonl", &[record]);
        let err = engine.recover().unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err:?}");
        assert!(app.exists());
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn cancelled_local_inspect_is_interrupted() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let cancel = Cancel::new();
    let engine = engine(root.path()).with_cancel(cancel.clone());
    let tarball = build_tarball(
        src.path(),
        "unpakt-fixture-con-1.0.tar.gz",
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-con/bin/unpakt-fixture-con",
            ELF,
            0o755,
        )],
    );
    cancel.cancel();
    let err = engine
        .inspect(tarball.to_str().unwrap(), AddOpts::default())
        .unwrap_err();
    assert!(matches!(err, Error::Interrupted), "{err:?}");
    assert!(tarball.exists());
}

#[test]
fn cancel_while_waiting_for_the_lock_changes_nothing() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut first = engine(root.path());
    let id = install_local(&mut first, src.path());
    let layout = first.layout().clone();
    let before = tree_snapshot(root.path());

    let held = lock::acquire(&layout.data.join("lock"), || {}).unwrap();
    let cancel = Cancel::new();
    let mut second = engine(root.path()).with_cancel(cancel.clone());
    let handle = {
        let id = id.clone();
        std::thread::spawn(move || second.remove(&id))
    };
    std::thread::sleep(Duration::from_millis(300));
    cancel.cancel();
    drop(held);
    let err = handle.join().unwrap().unwrap_err();
    assert!(matches!(err, Error::Interrupted), "{err:?}");
    assert_eq!(first.list().unwrap().len(), 1);
    assert_eq!(tree_snapshot(root.path()), before);
}
