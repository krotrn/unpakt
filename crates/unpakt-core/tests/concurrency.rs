mod common;

use std::ffi::OsString;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Comp, E, MockForge, build_appimage, build_tarball};
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::lock;
use unpakt_core::manifest::Manifest;
use unpakt_core::progress::{Progress, ProgressFn};
use unpakt_core::{Error, Layout};

const ELF: &[u8] = b"\x7fELF-fixture";
const DESKTOP: &[u8] = b"[Desktop Entry]\nType=Application\nName=Unpakt Fixture Con\nExec=AppRun %U\nIcon=unpakt-fixture-con\nCategories=Utility;\n";
const REPO: &str = "https://github.com/o/r";

fn engine(root: &Path) -> Engine {
    let layout = Layout::under(root);
    let bin = OsString::from(layout.bin.clone());
    Engine::new(layout, Host::detect())
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
