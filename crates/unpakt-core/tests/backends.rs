mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::{Comp, E, build_appimage, build_tarball, tree_snapshot};
use unpakt_core::backend::{Plan, for_payload};
use unpakt_core::format::detect;
use unpakt_core::model::Artifact;
use unpakt_core::{Error, Layout, Txn};

const ELF: &[u8] = b"\x7fELF-fixture";

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

fn layout(root: &std::path::Path) -> Layout {
    let l = Layout::under(root);
    std::fs::create_dir_all(&l.apps).unwrap();
    l
}

#[test]
fn appimage_direct_copies_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let image = build_appimage(
        src.path(),
        "Demo.AppImage",
        &[E::File("AppRun", ELF, 0o755)],
    );
    let plan = plan_for(image, "demo");
    let l = layout(tmp.path());
    let backend = for_payload(&plan.payload).unwrap();
    assert_eq!(backend.id(), "appimage-direct");

    let mut txn = Txn::new();
    let done = backend.install(&plan, &l, &mut txn).unwrap();
    txn.commit();

    assert_eq!(done.version_dir, l.apps.join("demo/1.0"));
    assert_eq!(done.entry, PathBuf::from("demo.AppImage"));
    let file = done.version_dir.join(&done.entry);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(&plan.artifact.path).unwrap()
    );
}

#[test]
fn existing_version_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let image = build_appimage(
        src.path(),
        "Demo.AppImage",
        &[E::File("AppRun", ELF, 0o755)],
    );
    let plan = plan_for(image, "demo");
    let l = layout(tmp.path());
    std::fs::create_dir_all(l.apps.join("demo/1.0")).unwrap();
    let mut txn = Txn::new();
    let err = for_payload(&plan.payload)
        .unwrap()
        .install(&plan, &l, &mut txn)
        .unwrap_err();
    assert!(matches!(err, Error::Conflict(_)));
}

#[test]
fn extract_preserves_exec_and_strips_setuid() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let tarball = build_tarball(
        src.path(),
        "tool-1.0.tar.gz",
        Comp::Gz,
        &[
            E::File("tool-1.0/bin/tool", ELF, 0o4755),
            E::File("tool-1.0/share/data.txt", b"data", 0o644),
            E::Link("tool-1.0/bin/alias", "tool"),
            E::Hard("tool-1.0/bin/hard", "tool-1.0/bin/tool"),
        ],
    );
    let plan = plan_for(tarball, "tool");
    let l = layout(tmp.path());
    let backend = for_payload(&plan.payload).unwrap();
    assert_eq!(backend.id(), "extract");

    let mut txn = Txn::new();
    let done = backend.install(&plan, &l, &mut txn).unwrap();
    txn.commit();

    assert_eq!(done.entry, PathBuf::from("bin/tool"));
    let bin = done.version_dir.join("bin/tool");
    assert_eq!(
        std::fs::metadata(&bin).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        std::fs::read(done.version_dir.join("share/data.txt")).unwrap(),
        b"data"
    );
    assert_eq!(
        std::fs::read_link(done.version_dir.join("bin/alias")).unwrap(),
        PathBuf::from("tool")
    );
    assert_eq!(
        std::fs::read(done.version_dir.join("bin/hard")).unwrap(),
        ELF
    );
    assert!(!l.apps.join("tool/.staging-1.0").exists());
}

fn hostile_cases() -> Vec<Vec<E<'static>>> {
    vec![
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::File("../x", b"evil", 0o644),
        ],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::File("/etc/evil", b"evil", 0o644),
        ],
        vec![E::File("app/bin/app", ELF, 0o755), E::Link("app/x", "/etc")],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Link("app/x", "../../outside"),
        ],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Link("app/d", "."),
            E::Link("app/d/l", "../../x"),
        ],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Link("app/d", ".."),
            E::File("app/d/escaped", b"evil", 0o644),
        ],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Link("app/a", "d/b/.."),
            E::Link("app/d/b", ".."),
        ],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Link("app/d", "bin"),
            E::File("app/d/f", b"evil", 0o644),
        ],
        vec![E::File("app/bin/app", ELF, 0o755), E::Hard("app/h", "../x")],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Hard("app/h", "/etc/passwd"),
        ],
    ]
}

#[test]
fn extract_never_writes_outside_version_dir() {
    let src = tempfile::tempdir().unwrap();
    let good = build_tarball(
        src.path(),
        "good.tar",
        Comp::None,
        &[E::File("app/bin/app", ELF, 0o755)],
    );
    for (i, entries) in hostile_cases().iter().enumerate() {
        let tmp = tempfile::tempdir().unwrap();
        let l = layout(tmp.path());
        let before = tree_snapshot(tmp.path());

        // Inspection would refuse these; the backend must hold on its own.
        let mut plan = plan_for(good.clone(), "app");
        plan.artifact.path =
            build_tarball(src.path(), &format!("bad-{i}.tar"), Comp::None, entries);
        let mut txn = Txn::new();
        let result = for_payload(&plan.payload)
            .unwrap()
            .install(&plan, &l, &mut txn);
        assert!(
            matches!(result, Err(Error::Refused(_))),
            "case {i}: {result:?}"
        );
        drop(txn);

        assert_eq!(tree_snapshot(tmp.path()), before, "case {i}");
        assert!(!tmp.path().join("x").exists() && !tmp.path().join(".local/share/x").exists());
    }
}

#[test]
fn failed_install_rolls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let tarball = build_tarball(
        src.path(),
        "tool-1.0.tar.gz",
        Comp::Gz,
        &[E::File("tool-1.0/bin/tool", ELF, 0o755)],
    );
    let plan = plan_for(tarball, "tool");
    let l = layout(tmp.path());
    let before = tree_snapshot(tmp.path());

    let mut txn = Txn::new();
    for_payload(&plan.payload)
        .unwrap()
        .install(&plan, &l, &mut txn)
        .unwrap();
    assert!(l.apps.join("tool/1.0/bin/tool").exists());
    drop(txn);

    assert!(!l.apps.join("tool").exists());
    assert_eq!(tree_snapshot(tmp.path()), before);
}

#[test]
fn extract_skips_pax_global_header_with_strip_prefix() {
    let src = tempfile::tempdir().unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    let comment = b"52 comment=0123456789abcdef0123456789abcdef01234567\n";
    let mut header = tar::Header::new_ustar();
    header.set_entry_type(tar::EntryType::XGlobalHeader);
    header.as_old_mut().name[..17].copy_from_slice(b"pax_global_header");
    header.set_size(comment.len() as u64);
    header.set_mode(0o666);
    header.set_cksum();
    builder.append(&header, &comment[..]).unwrap();
    let mut file = tar::Header::new_ustar();
    file.set_entry_type(tar::EntryType::Regular);
    file.as_old_mut().name[..11].copy_from_slice(b"app/bin/app");
    file.set_size(ELF.len() as u64);
    file.set_mode(0o755);
    file.set_cksum();
    builder.append(&file, ELF).unwrap();
    let path = src.path().join("git.tar");
    std::fs::write(&path, builder.into_inner().unwrap()).unwrap();

    let mut plan = plan_for(
        build_tarball(
            src.path(),
            "g.tar",
            Comp::None,
            &[E::File("app/bin/app", ELF, 0o755)],
        ),
        "app",
    );
    assert_eq!(
        plan.payload.strip_prefix.as_deref(),
        Some(std::path::Path::new("app"))
    );
    plan.artifact.path = path;
    let tmp = tempfile::tempdir().unwrap();
    let l = layout(tmp.path());
    let mut txn = Txn::new();
    let done = for_payload(&plan.payload)
        .unwrap()
        .install(&plan, &l, &mut txn)
        .unwrap();
    txn.commit();
    assert_eq!(
        std::fs::read(done.version_dir.join("bin/app")).unwrap(),
        ELF
    );
}
