mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{Comp, E, MockForge, build_appimage, build_tarball, tree_snapshot};
use unpakt_core::backend::{Plan, for_payload};
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::format::detect;
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::model::Artifact;
use unpakt_core::{Error, Layout, Txn};

const ELF: &[u8] = b"\x7fELF-fixture";
const REPO: &str = "https://github.com/o/r";

fn engine(root: &Path) -> Engine {
    let layout = Layout::under(root);
    let bin = OsString::from(layout.bin.clone());
    Engine::new(layout, Host::detect())
        .unwrap()
        .with_path_var(bin)
}

fn tool(dir: &Path, version: &str) -> PathBuf {
    build_tarball(
        dir,
        &format!("unpakt-fixture-rec-{version}.tar.gz"),
        Comp::Gz,
        &[
            E::File("unpakt-fixture-rec/bin/unpakt-fixture-rec", ELF, 0o755),
            E::File("unpakt-fixture-rec/README", version.as_bytes(), 0o644),
        ],
    )
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn plan_for(path: PathBuf, id: &str) -> Plan {
    let artifact = Artifact {
        file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
        path,
    };
    let payload = detect(&artifact).unwrap().inspect(&artifact).unwrap();
    let entry = payload.entries.first().cloned();
    Plan {
        id: id.into(),
        name: id.into(),
        artifact,
        payload,
        label: "1.0".into(),
        entry,
    }
}

#[test]
fn stale_staging_dir_does_not_block_add() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let app_dir = engine.layout().apps.join("unpakt-fixture-rec");
    std::fs::create_dir_all(app_dir.join(".staging-1.0")).unwrap();
    std::fs::write(app_dir.join(".staging-1.0/junk"), "half").unwrap();

    let plan = engine
        .inspect(
            tool(src.path(), "1.0").to_str().unwrap(),
            AddOpts::default(),
        )
        .unwrap();
    engine.install(plan).unwrap();
    assert_eq!(names(&app_dir), ["1.0", "current"]);
    assert_eq!(engine.list().unwrap().len(), 1);
}

#[test]
fn orphan_version_dir_without_a_row_does_not_block_add() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let app_dir = engine.layout().apps.join("unpakt-fixture-rec");
    std::fs::create_dir_all(app_dir.join("1.0")).unwrap();
    std::fs::write(app_dir.join("1.0/old"), "left behind").unwrap();
    std::fs::create_dir_all(app_dir.join("0.5")).unwrap();

    let plan = engine
        .inspect(
            tool(src.path(), "1.0").to_str().unwrap(),
            AddOpts::default(),
        )
        .unwrap();
    engine.install(plan).unwrap();
    assert_eq!(engine.list().unwrap().len(), 1);
    assert_eq!(names(&app_dir), ["1.0", "current"]);
    assert!(app_dir.join("1.0/bin/unpakt-fixture-rec").is_file());
    assert!(!app_dir.join("1.0/old").exists());
}

#[test]
fn stale_leftovers_are_restored_when_the_install_fails() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path());
    let layout = engine.layout().clone();
    let app_dir = layout.apps.join("unpakt-fixture-rec");
    std::fs::create_dir_all(app_dir.join("1.0")).unwrap();
    std::fs::write(app_dir.join("1.0/old"), "left behind").unwrap();
    // A foreign file at the command target makes the install fail late.
    std::fs::write(layout.bin.join("unpakt-fixture-rec"), "mine").unwrap();
    let before = tree_snapshot(root.path());

    let plan = engine
        .inspect(
            tool(src.path(), "1.0").to_str().unwrap(),
            AddOpts::default(),
        )
        .unwrap();
    assert!(matches!(engine.install(plan), Err(Error::Conflict(_))));
    assert_eq!(tree_snapshot(root.path()), before);
}

#[test]
fn update_clears_stale_dirs_the_manifest_does_not_know() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::under(root.path());
    let bin = OsString::from(layout.bin.clone());
    let mut engine = Engine::new(layout.clone(), Host::detect())
        .unwrap()
        .with_path_var(bin)
        .with_http(Http::new(&forge.api_url(), None).unwrap());

    let publish = |version: &str| {
        let name = format!("unpakt-fixture-rec-{version}.tar.gz");
        let path = tool(src.path(), version);
        forge.publish(version, vec![(name, std::fs::read(path).unwrap())]);
    };
    publish("1.0.0");
    let id = {
        let plan = engine.inspect(REPO, AddOpts::default()).unwrap();
        engine.install(plan).unwrap().id
    };
    let app_dir = layout.apps.join(&id);
    std::fs::create_dir_all(app_dir.join(".staging-2.0.0")).unwrap();
    std::fs::create_dir_all(app_dir.join("2.0.0/half")).unwrap();
    std::fs::create_dir_all(app_dir.join("9.9.9")).unwrap();

    publish("2.0.0");
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert_eq!(names(&app_dir), ["1.0.0", "2.0.0", "current"]);
    assert!(app_dir.join("2.0.0/bin/unpakt-fixture-rec").is_file());
}

#[test]
fn cancelled_extract_leaves_the_tree_untouched() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let l = Layout::under(root.path());
    std::fs::create_dir_all(&l.apps).unwrap();
    let before = tree_snapshot(root.path());

    let plan = plan_for(tool(src.path(), "1.0"), "unpakt-fixture-rec");
    let cancel = Cancel::new();
    cancel.cancel();
    let mut txn = Txn::with_cancel(cancel);
    let result = for_payload(&plan.payload)
        .unwrap()
        .install(&plan, &l, &mut txn);
    assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
    drop(txn);
    assert_eq!(tree_snapshot(root.path()), before);
}

#[test]
fn cancelled_appimage_copy_leaves_the_tree_untouched() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let l = Layout::under(root.path());
    std::fs::create_dir_all(&l.apps).unwrap();
    let before = tree_snapshot(root.path());

    let image = build_appimage(
        src.path(),
        "Demo-1.0.AppImage",
        &[E::File("AppRun", ELF, 0o755)],
    );
    let plan = plan_for(image, "demo");
    let cancel = Cancel::new();
    cancel.cancel();
    let mut txn = Txn::with_cancel(cancel);
    let result = for_payload(&plan.payload)
        .unwrap()
        .install(&plan, &l, &mut txn);
    assert!(matches!(result, Err(Error::Interrupted)), "{result:?}");
    drop(txn);
    assert_eq!(tree_snapshot(root.path()), before);
}

#[test]
fn cancelled_engine_install_and_download_leave_nothing_behind() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let cancel = Cancel::new();
    let layout = Layout::under(root.path());
    let bin = OsString::from(layout.bin.clone());
    let engine = Engine::new(layout.clone(), Host::detect())
        .unwrap()
        .with_path_var(bin)
        .with_http(Http::new(&forge.api_url(), None).unwrap())
        .with_cancel(cancel.clone());
    let name = "unpakt-fixture-rec-1.0.0.tar.gz";
    let path = tool(src.path(), "1.0.0");
    forge.publish("1.0.0", vec![(name.into(), std::fs::read(path).unwrap())]);
    std::fs::create_dir_all(&layout.cache).unwrap();
    let before = tree_snapshot(root.path());

    // Cancelled while downloading.
    cancel.cancel();
    let err = engine.inspect(REPO, AddOpts::default()).unwrap_err();
    assert!(matches!(err, Error::Interrupted), "{err:?}");
    assert_eq!(tree_snapshot(root.path()), before);
    let downloads = layout.cache.join("downloads");
    assert!(!downloads.exists() || names(&downloads).is_empty());

    // Cancelled between inspection and installation.
    let fresh = Cancel::new();
    let bin = OsString::from(layout.bin.clone());
    let mut engine = Engine::new(layout.clone(), Host::detect())
        .unwrap()
        .with_path_var(bin)
        .with_http(Http::new(&forge.api_url(), None).unwrap())
        .with_cancel(fresh.clone());
    let plan = engine.inspect(REPO, AddOpts::default()).unwrap();
    fresh.cancel();
    let err = engine.install(plan).unwrap_err();
    assert!(matches!(err, Error::Interrupted), "{err:?}");
    assert_eq!(tree_snapshot(root.path()), before);
    assert!(names(&downloads).is_empty());
    assert!(engine.list().unwrap().is_empty());
}

#[test]
fn old_download_dirs_are_swept_and_fresh_ones_kept() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::under(root.path());
    let bin = OsString::from(layout.bin.clone());
    let engine = Engine::new(layout.clone(), Host::detect())
        .unwrap()
        .with_path_var(bin)
        .with_http(Http::new(&forge.api_url(), None).unwrap());
    let name = "unpakt-fixture-rec-1.0.0.tar.gz";
    let path = tool(src.path(), "1.0.0");
    forge.publish("1.0.0", vec![(name.into(), std::fs::read(path).unwrap())]);

    let downloads = layout.cache.join("downloads");
    let old = downloads.join("dl-old");
    let fresh = downloads.join("dl-fresh");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::write(old.join("half"), "x").unwrap();
    let long_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 3600);
    std::fs::File::open(&old)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();

    let plan = engine.inspect(REPO, AddOpts::default()).unwrap();
    engine.discard(plan);
    assert!(!old.exists());
    assert!(fresh.exists());
}
