mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use common::{Comp, E, MockForge, build_appimage, build_tarball, tree_snapshot};
use unpakt_core::engine::{AddOpts, Engine, KEEP_VERSIONS, UpdateInfo};
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::{Error, Layout};

const REPO: &str = "https://github.com/o/r";
const DESKTOP: &[u8] = b"[Desktop Entry]\nType=Application\nName=Unpakt Fixture Upd\nExec=AppRun %U\nIcon=unpakt-fixture-upd\nCategories=Utility;\n";

fn engine(root: &Path, forge: &MockForge) -> Engine {
    let layout = Layout::under(root);
    let bin = OsString::from(layout.bin.clone());
    Engine::new(layout, Host::detect())
        .unwrap()
        .with_path_var(bin)
        .with_http(Http::new(&forge.api_url(), None).unwrap())
}

/// Publishes an AppImage whose payload differs per `version`.
fn publish(forge: &MockForge, tmp: &Path, version: &str) -> Vec<u8> {
    let name = format!("Unpakt-Fixture-Upd-{version}.AppImage");
    let elf = format!("\x7fELF-fixture-{version}");
    let path = build_appimage(
        tmp,
        &name,
        &[
            E::File("AppRun", elf.as_bytes(), 0o755),
            E::File("unpakt-fixture-upd.desktop", DESKTOP, 0o644),
        ],
    );
    let bytes = std::fs::read(path).unwrap();
    forge.publish(version, vec![(name, bytes.clone())]);
    bytes
}

fn validate(desktop: &Path) {
    let Ok(out) = Command::new("desktop-file-validate").arg(desktop).output() else {
        eprintln!("desktop-file-validate not installed; skipping");
        return;
    };
    assert!(
        out.status.success(),
        "desktop-file-validate: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn install_latest(engine: &mut Engine) -> String {
    let plan = engine.inspect(REPO, AddOpts::default()).unwrap();
    engine.install(plan).unwrap().id
}

fn exec_of(layout: &Layout, id: &str) -> String {
    let text =
        std::fs::read_to_string(layout.applications.join(format!("unpakt-{id}.desktop"))).unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix("Exec="))
        .unwrap()
        .to_string()
}

fn download_dirs(layout: &Layout) -> usize {
    std::fs::read_dir(layout.cache.join("downloads"))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[test]
fn update_check_update_rollback() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    let v1_bytes = publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
    assert!(engine.check_updates(Some(&id)).unwrap().updates.is_empty());
    assert_eq!(engine.update(&id).unwrap(), None);

    publish(&forge, src.path(), "2.0.0");
    assert_eq!(
        engine.check_updates(None).unwrap().updates,
        vec![UpdateInfo {
            id: id.clone(),
            current: "1.0.0".into(),
            latest: "2.0.0".into(),
        }]
    );

    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    let (app, versions, _) = engine.get(&id).unwrap();
    assert_eq!(app.current, "2.0.0");
    assert_eq!(versions.len(), 2);
    let desktop = layout.applications.join(format!("unpakt-{id}.desktop"));
    validate(&desktop);
    assert!(exec_of(&layout, &id).contains("apps"));
    assert!(
        layout
            .apps
            .join(&id)
            .join("current")
            .join(format!("{id}.AppImage"))
            .is_file()
    );
    assert_eq!(
        std::fs::read_link(layout.apps.join(&id).join("current")).unwrap(),
        Path::new("2.0.0")
    );
    assert!(std::fs::metadata(layout.bin.join(&id)).is_ok());
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
    assert_eq!(download_dirs(&layout), 0, "downloads are cleaned up");

    assert_eq!(engine.rollback(&id).unwrap(), "1.0.0");
    let (app, versions, _) = engine.get(&id).unwrap();
    assert_eq!(app.current, "1.0.0");
    assert_eq!(versions.len(), 2, "rollback deletes nothing");
    assert!(layout.apps.join(&id).join("2.0.0").is_dir());
    assert!(std::fs::metadata(layout.bin.join(&id)).is_ok());
    validate(&desktop);
    assert!(exec_of(&layout, &id).contains("%U"));
    assert!(
        engine
            .doctor()
            .iter()
            .all(|c| c.ok || c.name != format!("app:{id}"))
    );

    assert_eq!(
        std::fs::read_link(layout.apps.join(&id).join("current")).unwrap(),
        Path::new("1.0.0")
    );
    assert_eq!(std::fs::read(layout.bin.join(&id)).unwrap(), v1_bytes);
    let (_, _, recorded) = engine.get(&id).unwrap();
    let mut on_disk: Vec<_> = [&layout.bin, &layout.applications]
        .into_iter()
        .flat_map(|d| std::fs::read_dir(d).unwrap())
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(id.as_str())
        })
        .collect();
    on_disk.sort();
    let mut recorded: Vec<_> = recorded.into_iter().map(|f| f.path).collect();
    recorded.sort();
    assert_eq!(recorded, on_disk);

    assert!(matches!(engine.rollback(&id), Err(Error::NotFound(_))));
    // 2.0.0 is still on disk, so nothing new is installed.
    assert_eq!(engine.update(&id).unwrap(), None);
}

#[test]
fn keeps_three_versions() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);
    for v in ["2.0.0", "3.0.0", "4.0.0"] {
        publish(&forge, src.path(), v);
        assert_eq!(engine.update(&id).unwrap(), Some(v.into()));
    }
    let (app, versions, files) = engine.get(&id).unwrap();
    assert_eq!(KEEP_VERSIONS, 3);
    let labels: Vec<_> = versions.iter().map(|v| v.label.as_str()).collect();
    assert_eq!(labels, ["4.0.0", "3.0.0", "2.0.0"]);
    assert_eq!(app.current, "4.0.0");
    assert!(!layout.apps.join(&id).join("1.0.0").exists());
    assert!(layout.apps.join(&id).join("2.0.0").is_dir());
    assert!(
        files
            .iter()
            .all(|f| std::fs::symlink_metadata(&f.path).is_ok())
    );
    let leftovers: Vec<_> = std::fs::read_dir(layout.apps.join(&id))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn prune_never_removes_a_rolled_back_current() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);
    publish(&forge, src.path(), "2.0.0");
    engine.update(&id).unwrap();
    publish(&forge, src.path(), "3.0.0");
    engine.update(&id).unwrap();
    engine.rollback(&id).unwrap();
    engine.rollback(&id).unwrap();
    assert_eq!(engine.get(&id).unwrap().0.current, "1.0.0");

    publish(&forge, src.path(), "4.0.0");
    engine.update(&id).unwrap();
    let (app, versions, _) = engine.get(&id).unwrap();
    assert_eq!(app.current, "4.0.0");
    assert_eq!(versions.len(), KEEP_VERSIONS);
    for v in &versions {
        assert!(layout.apps.join(&id).join(&v.label).is_dir(), "{}", v.label);
    }
    assert!(
        layout
            .apps
            .join(&id)
            .join("current")
            .join(format!("{id}.AppImage"))
            .is_file()
    );
}

#[test]
fn truncated_download_leaves_current_intact() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);
    let before = tree_snapshot(root.path());

    publish(&forge, src.path(), "2.0.0");
    forge.set_truncate(true);
    assert!(engine.update(&id).is_err());

    assert_eq!(engine.get(&id).unwrap().0.current, "1.0.0");
    assert_eq!(engine.get(&id).unwrap().1.len(), 1);
    assert_eq!(tree_snapshot(root.path()), before);
    assert_eq!(download_dirs(&layout), 0);

    forge.set_truncate(false);
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
}

#[test]
fn failed_install_step_restores_everything() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);
    // A leftover directory with the new label makes the backend refuse.
    std::fs::create_dir(layout.apps.join(&id).join("2.0.0")).unwrap();
    let before = tree_snapshot(root.path());

    publish(&forge, src.path(), "2.0.0");
    assert!(matches!(engine.update(&id), Err(Error::Conflict(_))));
    assert_eq!(engine.get(&id).unwrap().0.current, "1.0.0");
    assert_eq!(tree_snapshot(root.path()), before);
    assert_eq!(download_dirs(&layout), 0);
}

#[test]
fn direct_url_marker_change_triggers_update() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);

    let name = "Unpakt-Fixture-Dir.AppImage";
    let build = |content: &str| {
        let path = build_appimage(
            src.path(),
            name,
            &[
                E::File("AppRun", content.as_bytes(), 0o755),
                E::File("unpakt-fixture-upd.desktop", DESKTOP, 0o644),
            ],
        );
        std::fs::read(path).unwrap()
    };
    forge.publish("-", vec![(name.into(), build("\x7fELF-one"))]);
    let url = forge.download_url(name);
    let plan = engine.inspect(&url, AddOpts::default()).unwrap();
    let id = engine.install(plan).unwrap().id;
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
    assert_eq!(engine.update(&id).unwrap(), None);

    forge.publish("-", vec![(name.into(), build("\x7fELF-two"))]);
    let found = engine.check_updates(None).unwrap().updates;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    let label = engine.update(&id).unwrap().expect("a new version");
    let (app, versions, _) = engine.get(&id).unwrap();
    assert_eq!(app.current, label);
    assert_eq!(versions.len(), 2);
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
}

#[test]
fn direct_url_without_marker_reports_nothing() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);

    let name = "Unpakt-Fixture-Nomark.AppImage";
    let path = build_appimage(
        src.path(),
        name,
        &[
            E::File("AppRun", b"\x7fELF-a", 0o755),
            E::File("unpakt-fixture-upd.desktop", DESKTOP, 0o644),
        ],
    );
    forge.publish("-", vec![(name.into(), std::fs::read(path).unwrap())]);
    let plan = engine
        .inspect(&forge.download_url(name), AddOpts::default())
        .unwrap();
    let id = engine.install(plan).unwrap().id;
    forge.set_head_status(405);
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
    // The content is unchanged, so the label already exists.
    assert_eq!(engine.update(&id).unwrap(), None);
}

#[test]
fn local_file_apps_are_skipped() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let path = build_appimage(
        src.path(),
        "Unpakt-Fixture-Local-1.0.AppImage",
        &[E::File("AppRun", b"\x7fELF-l", 0o755)],
    );
    let plan = engine
        .inspect(path.to_str().unwrap(), AddOpts::default())
        .unwrap();
    let id = engine.install(plan).unwrap().id;
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
    assert!(matches!(engine.update(&id), Err(Error::Unsupported(_))));
    assert!(path.is_file(), "a local source file is never deleted");
}

#[test]
fn older_latest_release_is_not_a_downgrade() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "2.0.0");
    let id = install_latest(&mut engine);
    publish(&forge, src.path(), "1.5.0");
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
    assert_eq!(engine.update(&id).unwrap(), None);
    let (app, versions, _) = engine.get(&id).unwrap();
    assert_eq!(app.current, "2.0.0");
    assert_eq!(versions.len(), 1);
    assert_eq!(download_dirs(&layout), 0);
}

#[test]
fn failed_check_is_reported_not_up_to_date() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine(root.path(), &forge));

    // Nothing listens on this API address.
    let layout = Layout::under(root.path());
    let bin = OsString::from(layout.bin.clone());
    let broken = Engine::new(layout, Host::detect())
        .unwrap()
        .with_path_var(bin)
        .with_http(Http::new("http://127.0.0.1:1", None).unwrap());
    for which in [None, Some(id.as_str())] {
        let check = broken.check_updates(which).unwrap();
        assert!(check.updates.is_empty());
        assert_eq!(check.failed.len(), 1);
        assert_eq!(check.failed[0].0, id);
    }
}

fn publish_tool(forge: &MockForge, tmp: &Path, version: &str, body: &[u8]) {
    let name = format!("unpakt-fixture-sw-{version}.tar.gz");
    let path = build_tarball(
        tmp,
        &name,
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-sw/bin/unpakt-fixture-sw",
            body,
            0o755,
        )],
    );
    forge.publish(version, vec![(name, std::fs::read(path).unwrap())]);
}

#[test]
fn rollback_restores_script_wrapper_and_update_restores_symlink() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish_tool(&forge, src.path(), "1.0.0", b"#!/bin/sh\necho one\n");
    let id = install_latest(&mut engine);
    let command = layout.bin.join("unpakt-fixture-sw");
    let is_link = |p: &Path| std::fs::symlink_metadata(p).unwrap().is_symlink();
    assert!(!is_link(&command), "a script gets a wrapper");

    publish_tool(&forge, src.path(), "2.0.0", b"\x7fELF-fixture-two");
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert!(is_link(&command), "an ELF entry is linked");
    assert_eq!(
        std::fs::read(&command).unwrap(),
        b"\x7fELF-fixture-two".to_vec()
    );

    assert_eq!(engine.rollback(&id).unwrap(), "1.0.0");
    assert!(!is_link(&command));
    let mode = std::fs::metadata(&command).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o755);
    let text = std::fs::read_to_string(&command).unwrap();
    let through_current = layout.apps.join(&id).join("current");
    assert!(
        text.contains(&format!("exec '{}/", through_current.display())),
        "{text}"
    );
    assert!(text.contains("bin/unpakt-fixture-sw"), "{text}");
    assert!(
        std::fs::read_to_string(through_current.join("bin/unpakt-fixture-sw"))
            .unwrap()
            .contains("echo one")
    );
}

#[test]
fn forced_shadowed_command_survives_update_and_rollback() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::under(root.path());
    let other = root.path().join("other-bin");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("unpakt-fixture-upd"), "system tool").unwrap();
    let path_var = std::env::join_paths([&other, &layout.bin]).unwrap();
    let mut engine = Engine::new(layout.clone(), Host::detect())
        .unwrap()
        .with_path_var(path_var)
        .with_http(Http::new(&forge.api_url(), None).unwrap());

    publish(&forge, src.path(), "1.0.0");
    let plain = engine.inspect(REPO, AddOpts::default()).unwrap();
    assert!(matches!(engine.install(plain), Err(Error::Conflict(_))));
    let forced = AddOpts {
        force: true,
        ..AddOpts::default()
    };
    let plan = engine.inspect(REPO, forced).unwrap();
    let id = engine.install(plan).unwrap().id;

    publish(&forge, src.path(), "2.0.0");
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert_eq!(engine.rollback(&id).unwrap(), "1.0.0");
    assert_eq!(engine.get(&id).unwrap().0.current, "1.0.0");
}
