mod common;

use common::tree_snapshot;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use unpakt_core::Error;
use unpakt_core::Layout;
use unpakt_core::Txn;
use unpakt_core::integrate::{IntegrateReq, disintegrate, find_shadow, integrate};
use unpakt_core::manifest::{FileKind, FileRecord};
use unpakt_core::model::{ExecKind, Icon};

fn png(width: u32) -> Icon {
    let mut data = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    data.extend_from_slice(b"IHDR");
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&width.to_be_bytes());
    Icon {
        ext: "png".into(),
        data,
    }
}

fn req<'a>(exec: &'a Path, path_var: &'a std::ffi::OsStr) -> IntegrateReq<'a> {
    IntegrateReq {
        id: "x",
        name: "Fixture X",
        exec,
        kind: ExecKind::Elf,
        desktop_src: None,
        icon: None,
        want_desktop: true,
        command: Some("unpakt-fixture-foo"),
        force: false,
        no_fuse_appimage: false,
        owned: &[],
        path_var,
    }
}

fn setup() -> (tempfile::TempDir, Layout, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::under(dir.path());
    for d in [&layout.applications, &layout.bin, &layout.icons] {
        fs::create_dir_all(d).unwrap();
    }
    let exec = layout.apps.join("x/current/app");
    (dir, layout, exec)
}

#[test]
fn writes_valid_desktop_and_icon() {
    let (_dir, layout, exec) = setup();
    let icon = png(128);
    let pv = OsString::from(&layout.bin);
    let mut r = req(&exec, &pv);
    r.icon = Some(&icon);
    r.no_fuse_appimage = true;
    r.desktop_src = Some(
        "[Desktop Entry]\nName=Orig\nExec=orig --flag %U\nTryExec=orig\nDBusActivatable=true\nIcon=orig\n",
    );
    let mut txn = Txn::new();
    let out = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();

    let desktop = layout.applications.join("unpakt-x.desktop");
    let icon_path = layout.icons.join("128x128/apps/unpakt-x.png");
    assert!(icon_path.is_file());
    let text = fs::read_to_string(&desktop).unwrap();
    assert!(text.contains("Icon=unpakt-x\n"), "{text}");
    assert!(text.contains("X-Unpakt-Id=x\n"));
    assert!(
        text.contains("Exec=env APPIMAGE_EXTRACT_AND_RUN=1 "),
        "{text}"
    );
    assert!(text.contains(" %U"));
    assert!(!text.contains("TryExec") && !text.contains("DBusActivatable"));
    assert_eq!(
        fs::metadata(&desktop).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(out.files.contains(&FileRecord {
        path: desktop.clone(),
        kind: FileKind::Desktop
    }));
    assert!(out.files.contains(&FileRecord {
        path: icon_path,
        kind: FileKind::Icon
    }));
    assert!(out.files.iter().any(|f| f.kind == FileKind::Bin));

    match std::process::Command::new("desktop-file-validate")
        .arg(&desktop)
        .status()
    {
        Ok(s) => assert!(s.success(), "desktop-file-validate failed on:\n{text}"),
        Err(_) => eprintln!("desktop-file-validate not installed; skipping validation"),
    }
}

#[test]
fn icon_sizes_and_formats() {
    for (icon, expect) in [
        (png(100), "128x128/apps/unpakt-x.png"),
        (png(1000), "512x512/apps/unpakt-x.png"),
        (png(16), "16x16/apps/unpakt-x.png"),
        (
            Icon {
                ext: "svg".into(),
                data: b"<svg/>".to_vec(),
            },
            "scalable/apps/unpakt-x.svg",
        ),
    ] {
        let (_dir, layout, exec) = setup();
        let pv = OsString::from(&layout.bin);
        let mut r = req(&exec, &pv);
        r.icon = Some(&icon);
        r.command = None;
        let mut txn = Txn::new();
        integrate(&layout, &r, &mut txn).unwrap();
        txn.commit();
        assert!(layout.icons.join(expect).is_file(), "{expect}");
    }
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let xpm = Icon {
        ext: "xpm".into(),
        data: b"/* XPM */".to_vec(),
    };
    let mut r = req(&exec, &pv);
    r.icon = Some(&xpm);
    r.command = None;
    let mut txn = Txn::new();
    let out = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    assert!(out.warnings.iter().any(|w| w.contains("xpm")));
    let text = fs::read_to_string(layout.applications.join("unpakt-x.desktop")).unwrap();
    assert!(!text.contains("Icon="));
}

#[test]
fn script_entry_gets_wrapper_elf_gets_symlink() {
    let (_dir, layout, _) = setup();
    let pv = OsString::from(&layout.bin);
    let exec = layout.apps.join("it's/current/run");
    let mut r = req(&exec, &pv);
    r.kind = ExecKind::Script;
    let mut txn = Txn::new();
    integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    let bin = layout.bin.join("unpakt-fixture-foo");
    let body = fs::read_to_string(&bin).unwrap();
    assert_eq!(
        body,
        format!(
            "#!/bin/sh\nexec '{}' \"$@\"\n",
            exec.display().to_string().replace('\'', "'\\''")
        )
    );
    assert_eq!(
        fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
        0o755
    );

    let (_dir2, layout2, exec2) = setup();
    let pv2 = OsString::from(&layout2.bin);
    let r2 = req(&exec2, &pv2);
    let mut txn = Txn::new();
    integrate(&layout2, &r2, &mut txn).unwrap();
    txn.commit();
    assert_eq!(
        fs::read_link(layout2.bin.join("unpakt-fixture-foo")).unwrap(),
        exec2
    );
}

#[test]
fn foreign_bin_file_is_not_overwritten() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let bin = layout.bin.join("unpakt-fixture-foo");
    fs::write(&bin, "mine").unwrap();
    let before = tree_snapshot(_dir.path());

    let mut txn = Txn::new();
    let err = integrate(&layout, &req(&exec, &pv), &mut txn).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    drop(txn);
    assert_eq!(fs::read_to_string(&bin).unwrap(), "mine");
    assert_eq!(tree_snapshot(_dir.path()), before);

    let mut r = req(&exec, &pv);
    r.force = true;
    let mut txn = Txn::new();
    integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    assert_eq!(fs::read_link(&bin).unwrap(), exec);
}

#[test]
fn foreign_desktop_file_is_not_overwritten() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let desktop = layout.applications.join("unpakt-x.desktop");
    fs::write(&desktop, "mine").unwrap();
    let mut txn = Txn::new();
    let err = integrate(&layout, &req(&exec, &pv), &mut txn).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)));
    drop(txn);
    assert_eq!(fs::read_to_string(&desktop).unwrap(), "mine");
}

#[test]
fn forced_replacement_is_undone_on_rollback() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    fs::write(layout.bin.join("unpakt-fixture-foo"), "mine").unwrap();
    let before = tree_snapshot(_dir.path());
    let mut r = req(&exec, &pv);
    r.force = true;
    let mut txn = Txn::new();
    integrate(&layout, &r, &mut txn).unwrap();
    drop(txn);
    assert_eq!(tree_snapshot(_dir.path()), before);
}

#[test]
fn shadowed_command_refused() {
    let (dir, layout, exec) = setup();
    let other = dir.path().join("other-bin");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("unpakt-fixture-foo"), "x").unwrap();
    let pv = std::env::join_paths([&other, &layout.bin]).unwrap();
    let mut txn = Txn::new();
    let err = integrate(&layout, &req(&exec, &pv), &mut txn).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    drop(txn);
    assert!(!layout.bin.join("unpakt-fixture-foo").exists());

    assert_eq!(
        find_shadow("unpakt-fixture-foo", &pv, &layout.bin),
        Some(other.clone())
    );
    // A dir after our bin dir does not shadow it.
    let pv_after = std::env::join_paths([&layout.bin, &other]).unwrap();
    assert_eq!(
        find_shadow("unpakt-fixture-foo", &pv_after, &layout.bin),
        None
    );
    // When our bin dir is not on PATH, every dir counts.
    let pv_none = std::env::join_paths([&other]).unwrap();
    assert_eq!(
        find_shadow("unpakt-fixture-foo", &pv_none, &layout.bin),
        Some(other.clone())
    );

    let mut r = req(&exec, &pv);
    r.force = true;
    let mut txn = Txn::new();
    integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
}

#[test]
fn bin_not_on_path_warns() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from("/usr/bin");
    let mut txn = Txn::new();
    let out = integrate(&layout, &req(&exec, &pv), &mut txn).unwrap();
    txn.commit();
    let want = format!("{} is not in PATH", layout.bin.display());
    assert!(out.warnings.contains(&want), "{:?}", out.warnings);
}

#[test]
fn reintegrate_with_owned_files_succeeds() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let icon = png(48);
    let mut r = req(&exec, &pv);
    r.icon = Some(&icon);
    let mut txn = Txn::new();
    let first = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();

    let exec2 = layout.apps.join("x/current/app2");
    let mut r2 = req(&exec2, &pv);
    r2.icon = Some(&icon);
    r2.owned = &first.files;
    let mut txn = Txn::new();
    let second = integrate(&layout, &r2, &mut txn).unwrap();
    txn.commit();
    assert_eq!(first.files.len(), second.files.len());
    assert_eq!(
        fs::read_link(layout.bin.join("unpakt-fixture-foo")).unwrap(),
        exec2
    );

    // Switching from a wrapper to a link over an owned file works too.
    let mut r3 = req(&exec2, &pv);
    r3.kind = ExecKind::Script;
    r3.owned = &second.files;
    let mut txn = Txn::new();
    let third = integrate(&layout, &r3, &mut txn).unwrap();
    txn.commit();
    let mut r4 = req(&exec2, &pv);
    r4.owned = &third.files;
    let mut txn = Txn::new();
    integrate(&layout, &r4, &mut txn).unwrap();
    txn.commit();
    assert!(
        fs::symlink_metadata(layout.bin.join("unpakt-fixture-foo"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn disintegrate_removes_all() {
    let (dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let before = tree_snapshot(dir.path());
    for icon in [
        png(128),
        Icon {
            ext: "svg".into(),
            data: b"<svg/>".to_vec(),
        },
    ] {
        let mut r = req(&exec, &pv);
        r.icon = Some(&icon);
        let mut txn = Txn::new();
        let out = integrate(&layout, &r, &mut txn).unwrap();
        txn.commit();

        let mut txn = Txn::new();
        disintegrate(&out.files, &mut txn).unwrap();
        txn.commit();
        assert_eq!(tree_snapshot(dir.path()), before);

        // Already-missing files are fine.
        let mut txn = Txn::new();
        disintegrate(&out.files, &mut txn).unwrap();
        txn.commit();
    }
}

#[test]
fn disintegrate_keeps_dirs_with_other_content() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let icon = png(48);
    let mut r = req(&exec, &pv);
    r.icon = Some(&icon);
    let mut txn = Txn::new();
    let out = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    let other = layout.icons.join("48x48/apps/other.png");
    fs::write(&other, "o").unwrap();
    let mut txn = Txn::new();
    disintegrate(&out.files, &mut txn).unwrap();
    txn.commit();
    assert!(other.is_file());
}

#[test]
fn disintegrate_keeps_dir_with_user_unpakt_file() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let icon = png(48);
    let mut r = req(&exec, &pv);
    r.icon = Some(&icon);
    let mut txn = Txn::new();
    let out = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    let user = layout.icons.join("48x48/apps/.unpakt-bak-user");
    fs::write(&user, "u").unwrap();
    let mut txn = Txn::new();
    disintegrate(&out.files, &mut txn).unwrap();
    txn.commit();
    assert!(user.is_file());
}

struct ErrorCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl tracing::Subscriber for ErrorCounter {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::ERROR {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn commit_after_pruning_logs_no_errors() {
    let (_dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let icon = png(48);
    let mut r = req(&exec, &pv);
    r.icon = Some(&icon);
    let mut txn = Txn::new();
    let out = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tracing::subscriber::with_default(ErrorCounter(count.clone()), || {
        let mut txn = Txn::new();
        disintegrate(&out.files, &mut txn).unwrap();
        txn.commit();
    });
    assert!(!layout.icons.join("48x48").exists());
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn hostile_command_names_are_refused() {
    let (dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let victim = dir.path().join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("keep"), "precious").unwrap();
    let before = tree_snapshot(dir.path());
    let absolute = victim.to_string_lossy().into_owned();
    for command in [
        "../../victim",
        "..",
        ".",
        "",
        "a/b",
        "sub/",
        "nul\0byte",
        absolute.as_str(),
    ] {
        for force in [false, true] {
            let mut r = req(&exec, &pv);
            r.command = Some(command);
            r.force = force;
            let mut txn = Txn::new();
            let err = integrate(&layout, &r, &mut txn).unwrap_err();
            assert!(matches!(err, Error::Refused(_)), "{command:?}: {err:?}");
            drop(txn);
        }
    }
    assert_eq!(tree_snapshot(dir.path()), before);
}

#[test]
fn force_never_replaces_a_directory_at_the_bin_target() {
    let (dir, layout, exec) = setup();
    let pv = OsString::from(&layout.bin);
    let target = layout.bin.join("unpakt-fixture-foo");
    fs::create_dir_all(target.join("inner")).unwrap();
    fs::write(target.join("inner/keep"), "precious").unwrap();
    let before = tree_snapshot(dir.path());

    let mut r = req(&exec, &pv);
    r.force = true;
    let mut txn = Txn::new();
    let err = integrate(&layout, &r, &mut txn).unwrap_err();
    assert!(
        matches!(err, Error::Refused(_) | Error::Conflict(_)),
        "{err:?}"
    );
    drop(txn);
    assert_eq!(tree_snapshot(dir.path()), before);
    assert_eq!(
        fs::read_to_string(target.join("inner/keep")).unwrap(),
        "precious"
    );
}

#[test]
fn no_fuse_command_is_a_wrapper_that_sets_extract_and_run() {
    let (_dir, layout, exec) = setup();
    fs::create_dir_all(exec.parent().unwrap()).unwrap();
    fs::write(
        &exec,
        "#!/bin/sh\necho \"extract=$APPIMAGE_EXTRACT_AND_RUN args=$*\"\n",
    )
    .unwrap();
    fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).unwrap();
    let pv = OsString::from(&layout.bin);
    let bin = layout.bin.join("unpakt-fixture-foo");

    let run = || {
        let out = std::process::Command::new("sh")
            .arg(&bin)
            .args(["one", "two words"])
            .env_clear()
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };

    let mut r = req(&exec, &pv);
    r.no_fuse_appimage = true;
    let mut txn = Txn::new();
    let first = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    assert!(!fs::symlink_metadata(&bin).unwrap().is_symlink());
    assert_eq!(run(), "extract=1 args=one two words\n");

    // Re-integrating over the owned wrapper, and switching back to a plain link.
    r.owned = &first.files;
    let mut txn = Txn::new();
    let second = integrate(&layout, &r, &mut txn).unwrap();
    txn.commit();
    assert_eq!(run(), "extract=1 args=one two words\n");

    let mut plain = req(&exec, &pv);
    plain.owned = &second.files;
    let mut txn = Txn::new();
    integrate(&layout, &plain, &mut txn).unwrap();
    txn.commit();
    assert!(fs::symlink_metadata(&bin).unwrap().is_symlink());
}
