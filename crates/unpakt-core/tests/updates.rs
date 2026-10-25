mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use common::{Comp, E, MockForge, build_appimage, build_tarball, tree_snapshot};
use unpakt_core::cancel::Cancel;
use unpakt_core::engine::{AddOpts, Engine, KEEP_VERSIONS, UpdateInfo};
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::{Error, Layout};

const REPO: &str = "https://github.com/o/r";
const DESKTOP: &[u8] = b"[Desktop Entry]\nType=Application\nName=Unpakt Fixture Upd\nExec=AppRun %U\nIcon=unpakt-fixture-upd\nCategories=Utility;\n";

fn engine(root: &Path, forge: &MockForge) -> Engine {
    engine_with(root, forge, true)
}

fn engine_with(root: &Path, forge: &MockForge, fuse2: bool) -> Engine {
    let layout = Layout::under(root);
    let bin = OsString::from(layout.bin.clone());
    Engine::new(
        layout,
        Host {
            fuse2,
            ..Host::detect()
        },
    )
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
fn host_without_fuse_installs_a_wrapper_script() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine_with(root.path(), &forge, false);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);

    let entry = layout.bin.join(&id);
    assert!(
        !std::fs::symlink_metadata(&entry).unwrap().is_symlink(),
        "no-FUSE hosts get a wrapper, not a link"
    );
    let script = std::fs::read_to_string(&entry).unwrap();
    assert!(
        script.contains("exec env APPIMAGE_EXTRACT_AND_RUN=1 "),
        "{script}"
    );
    assert!(
        exec_of(&layout, &id).starts_with("env APPIMAGE_EXTRACT_AND_RUN=1 "),
        "{}",
        exec_of(&layout, &id)
    );
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
}

#[test]
fn update_after_rollback_switches_back_without_downloading() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);
    publish(&forge, src.path(), "2.0.0");
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert_eq!(engine.rollback(&id).unwrap(), "1.0.0");

    let asset = "/dl/Unpakt-Fixture-Upd-2.0.0.AppImage";
    let downloads = forge.request_count(asset);
    let found = engine.check_updates(None).unwrap().updates;
    assert_eq!(
        found,
        vec![UpdateInfo {
            id: id.clone(),
            current: "1.0.0".into(),
            latest: "2.0.0".into(),
        }]
    );
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert_eq!(
        forge.request_count(asset),
        downloads,
        "nothing is fetched again"
    );
    let (app, versions, _) = engine.get(&id).unwrap();
    assert_eq!(app.current, "2.0.0");
    assert_eq!(versions.len(), 2);
    assert_eq!(
        std::fs::read_link(layout.apps.join(&id).join("current")).unwrap(),
        Path::new("2.0.0")
    );
    assert!(engine.check_updates(None).unwrap().updates.is_empty());
    assert_eq!(engine.update(&id).unwrap(), None);
    assert_eq!(download_dirs(&layout), 0);
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
    // A stray file with the new label (never cleared as a stale version
    // directory) makes the backend refuse.
    std::fs::write(layout.apps.join(&id).join("2.0.0"), "stray").unwrap();
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
    let broken = Engine::new(
        layout,
        Host {
            fuse2: true,
            ..Host::detect()
        },
    )
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
    let mut engine = Engine::new(
        layout.clone(),
        Host {
            fuse2: true,
            ..Host::detect()
        },
    )
    .unwrap()
    .with_path_var(path_var)
    .with_http(Http::new(&forge.api_url(), None).unwrap());

    publish(&forge, src.path(), "1.0.0");
    let plain = engine.inspect(REPO, AddOpts::default()).unwrap();
    assert!(matches!(engine.install(plain), Err(Error::Shadowed { .. })));
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

fn png(width: u32) -> Vec<u8> {
    let mut data = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    data.extend_from_slice(b"IHDR");
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&width.to_be_bytes());
    data
}

fn publish_with_icon(forge: &MockForge, tmp: &Path, version: &str, icon: Option<&[u8]>) {
    let name = format!("Unpakt-Fixture-Upd-{version}.AppImage");
    let elf = format!("\x7fELF-fixture-{version}");
    let mut files = vec![
        E::File("AppRun", elf.as_bytes(), 0o755),
        E::File("unpakt-fixture-upd.desktop", DESKTOP, 0o644),
    ];
    if let Some(icon) = icon {
        files.push(E::File("unpakt-fixture-upd.png", icon, 0o644));
    }
    let path = build_appimage(tmp, &name, &files);
    forge.publish(version, vec![(name, std::fs::read(path).unwrap())]);
}

fn icon_files(layout: &Layout) -> Vec<String> {
    tree_snapshot(&layout.icons)
        .into_iter()
        .filter(|(_, v)| v.starts_with("file:"))
        .map(|(p, _)| p.display().to_string())
        .collect()
}

#[test]
fn update_drops_files_the_new_version_no_longer_ships() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();
    // Downloads create the cache dir, which stays behind by design.
    std::fs::create_dir_all(&layout.cache).unwrap();
    let baseline = tree_snapshot(root.path());

    publish_with_icon(&forge, src.path(), "1.0.0", Some(&png(16)));
    let id = install_latest(&mut engine);
    assert_eq!(icon_files(&layout).len(), 1);

    publish_with_icon(&forge, src.path(), "2.0.0", Some(&png(256)));
    engine.update(&id).unwrap();
    let icons = icon_files(&layout);
    assert_eq!(icons.len(), 1, "{icons:?}");
    assert!(icons[0].contains("256x256"), "{icons:?}");
    assert!(!layout.icons.join("16x16").exists());
    let recorded = engine.get(&id).unwrap().2;
    assert!(
        recorded
            .iter()
            .all(|f| std::fs::symlink_metadata(&f.path).is_ok())
    );

    publish_with_icon(&forge, src.path(), "3.0.0", None);
    engine.update(&id).unwrap();
    assert!(icon_files(&layout).is_empty(), "{:?}", icon_files(&layout));
    assert!(!layout.icons.join("256x256").exists());

    engine.remove(&id).unwrap();
    assert_eq!(tree_snapshot(root.path()), baseline);
}

#[test]
fn rollback_with_cancel_set_is_undone() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let cancel = Cancel::new();
    let mut engine = engine(root.path(), &forge).with_cancel(cancel.clone());

    publish(&forge, src.path(), "1.0.0");
    let id = install_latest(&mut engine);
    publish(&forge, src.path(), "2.0.0");
    engine.update(&id).unwrap();
    let before = tree_snapshot(root.path());

    cancel.cancel();
    assert!(matches!(engine.rollback(&id), Err(Error::Interrupted)));
    assert_eq!(tree_snapshot(root.path()), before);
    assert_eq!(engine.get(&id).unwrap().0.current, "2.0.0");
}

fn name_of(layout: &Layout, id: &str) -> String {
    let text =
        std::fs::read_to_string(layout.applications.join(format!("unpakt-{id}.desktop"))).unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix("Name="))
        .unwrap()
        .to_string()
}

#[test]
fn id_suffix_survives_rollback_and_reactivation_once() {
    let forge = MockForge::start();
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut engine = engine(root.path(), &forge);
    let layout = engine.layout().clone();

    publish(&forge, src.path(), "1.0.0");
    let opts = AddOpts {
        id: Some("fixture-alt".into()),
        ..AddOpts::default()
    };
    let plan = engine.inspect(REPO, opts).unwrap();
    let id = engine.install(plan).unwrap().id;
    let want = "Unpakt Fixture Upd (fixture-alt)";
    assert_eq!(name_of(&layout, &id), want);

    publish(&forge, src.path(), "2.0.0");
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert_eq!(name_of(&layout, &id), want);
    assert_eq!(engine.rollback(&id).unwrap(), "1.0.0");
    assert_eq!(name_of(&layout, &id), want);
    assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    assert_eq!(name_of(&layout, &id), want);
    validate(&layout.applications.join(format!("unpakt-{id}.desktop")));
}

mod package_updates {
    use super::common::{Comp, E, MockForge, build_appimage, build_tarball, deb_with_elf};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use unpakt_core::compat::HostLibs;
    use unpakt_core::engine::{AddOpts, Engine};
    use unpakt_core::http::Http;
    use unpakt_core::manifest::Manifest;
    use unpakt_core::model::FormatKind;
    use unpakt_core::{Error, Layout, host::Host};

    const REPO: &str = "https://github.com/o/r";

    fn engine(root: &Path, forge: &MockForge) -> Engine {
        let layout = Layout::under(root);
        let bin = OsString::from(layout.bin.clone());
        let libs = HostLibs::from_parts(
            BTreeMap::from([("libc.so.6".to_string(), PathBuf::from("/lib/libc.so.6"))]),
            BTreeMap::from([(
                "libc.so.6".to_string(),
                BTreeSet::from(["GLIBC_2.34".to_string()]),
            )]),
            true,
        );
        Engine::new(
            layout,
            Host {
                fuse2: true,
                ..Host::detect()
            },
        )
        .unwrap()
        .with_path_var(bin)
        .with_host_libs(libs)
        .with_http(Http::new(&forge.api_url(), None).unwrap())
    }

    fn option(root: &Path, id: &str, key: &str) -> Option<String> {
        Manifest::open(&Layout::under(root).db)
            .unwrap()
            .option(id, key)
            .unwrap()
    }

    fn tarball(dir: &Path, version: &str) -> (String, Vec<u8>) {
        let name = format!("app-{version}.tar.gz");
        let body = format!("\x7fELF-{version}");
        let path = build_tarball(
            dir,
            &name,
            Comp::Gz,
            &[E::File("app/app", body.as_bytes(), 0o755)],
        );
        (name, std::fs::read(path).unwrap())
    }

    fn appimage(dir: &Path, version: &str) -> (String, Vec<u8>) {
        let name = format!("app-{version}.AppImage");
        let body = format!("\x7fELF-{version}");
        let path = build_appimage(dir, &name, &[E::File("AppRun", body.as_bytes(), 0o755)]);
        (name, std::fs::read(path).unwrap())
    }

    fn deb(dir: &Path, version: &str, unit: bool, needs: &[&str]) -> (String, Vec<u8>) {
        let (name, path) = deb_with_elf(dir, version, needs, &[], unit);
        (name, std::fs::read(path).unwrap())
    }

    fn install_risky(engine: &mut Engine) -> String {
        let opts = AddOpts {
            allow_risky: true,
            ..AddOpts::default()
        };
        let plan = engine.inspect(REPO, opts).unwrap();
        engine.install(plan).unwrap().id
    }

    #[test]
    fn update_keeps_installed_format() {
        let forge = MockForge::start();
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut engine = engine(root.path(), &forge);
        forge.publish("1.0.0", vec![tarball(src.path(), "1.0")]);
        let plan = engine.inspect(REPO, AddOpts::default()).unwrap();
        let id = engine.install(plan).unwrap().id;

        forge.publish(
            "2.0.0",
            vec![tarball(src.path(), "2.0"), appimage(src.path(), "2.0")],
        );
        assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
        assert_eq!(engine.get(&id).unwrap().0.format, FormatKind::Tarball);
    }

    #[test]
    fn update_reports_missing_format() {
        let forge = MockForge::start();
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut engine = engine(root.path(), &forge);
        forge.publish("1.0.0", vec![tarball(src.path(), "1.0")]);
        let plan = engine.inspect(REPO, AddOpts::default()).unwrap();
        let id = engine.install(plan).unwrap().id;

        forge.publish("2.0.0", vec![appimage(src.path(), "2.0")]);
        match engine.update(&id) {
            Err(Error::NotFound(m)) => assert!(m.contains("tarball"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(engine.get(&id).unwrap().0.current, "1.0.0");
    }

    #[test]
    fn risky_update_with_accepted_tags_proceeds() {
        let forge = MockForge::start();
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut engine = engine(root.path(), &forge);
        forge.publish(
            "1.0.0",
            vec![deb(src.path(), "1.0.0", false, &["libnotthere.so.9"])],
        );
        let id = install_risky(&mut engine);

        forge.publish(
            "2.0.0",
            vec![deb(src.path(), "2.0.0", false, &["libnotthere.so.9"])],
        );
        assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
    }

    #[test]
    fn risky_update_with_new_tag_keeps_current() {
        let forge = MockForge::start();
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut engine = engine(root.path(), &forge);
        forge.publish(
            "1.0.0",
            vec![deb(src.path(), "1.0.0", false, &["libnotthere.so.9"])],
        );
        let id = install_risky(&mut engine);

        forge.publish(
            "2.0.0",
            vec![deb(src.path(), "2.0.0", true, &["libnotthere.so.9"])],
        );
        match engine.update(&id) {
            Err(Error::Refused(m)) => {
                assert!(m.contains("system-integration"), "{m}");
                assert!(m.contains("--allow-risky"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(engine.get(&id).unwrap().0.current, "1.0.0");

        assert_eq!(engine.update_with(&id, true).unwrap(), Some("2.0.0".into()));
        assert_eq!(
            option(root.path(), &id, "compat.accepted").as_deref(),
            Some("missing-library,system-integration")
        );
    }

    #[test]
    fn needs_container_update_keeps_current() {
        let forge = MockForge::start();
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut engine = engine(root.path(), &forge);
        forge.publish(
            "1.0.0",
            vec![deb(src.path(), "1.0.0", false, &["libc.so.6"])],
        );
        let plan = engine.inspect(REPO, AddOpts::default()).unwrap();
        let id = engine.install(plan).unwrap().id;

        let (name, path) = deb_with_elf(
            src.path(),
            "2.0.0",
            &["libc.so.6"],
            &[("libc.so.6", "GLIBC_9.99")],
            false,
        );
        forge.publish("2.0.0", vec![(name, std::fs::read(path).unwrap())]);
        for allow in [false, true] {
            match engine.update_with(&id, allow) {
                Err(Error::Refused(m)) => assert!(m.contains("current version kept"), "{m}"),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(engine.get(&id).unwrap().0.current, "1.0.0");
    }
}
