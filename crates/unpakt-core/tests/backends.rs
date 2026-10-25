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

mod packages {
    use super::common::{E, RpmTags, build_elf, build_rpm, electron_like, host_rpm_arch};
    use super::{layout, plan_for};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use unpakt_core::backend::{Installed, for_payload};
    use unpakt_core::model::FormatKind;
    use unpakt_core::{Error, Layout, Txn};

    fn install(root: &Path, artifact: PathBuf) -> unpakt_core::Result<(Layout, Installed)> {
        let plan = plan_for(artifact, "foo");
        let l = layout(root);
        let backend = for_payload(&plan.payload).expect("a backend for packages");
        assert_eq!(backend.id(), "extract");
        let mut txn = Txn::new();
        let done = backend.install(&plan, &l, &mut txn)?;
        txn.commit();
        Ok((l, done))
    }

    #[test]
    fn deb_extracts_into_version_dir_with_rebased_links() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let deb = electron_like(src.path(), FormatKind::Deb);
        let (l, done) = install(root.path(), deb).unwrap();
        let dir = &done.version_dir;

        assert_eq!(dir, &l.apps.join("foo/1.0"));
        assert!(dir.join("opt/Foo/foo").is_file());
        assert_eq!(done.entry, PathBuf::from("opt/Foo/foo"));
        assert_eq!(
            std::fs::read_link(dir.join("usr/bin/foo")).unwrap(),
            PathBuf::from("../../opt/Foo/foo")
        );
        let mode = std::fs::metadata(dir.join("opt/Foo/chrome-sandbox"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o6000, 0);
        assert_eq!(mode & 0o777, 0o755);
    }

    fn assert_extracted(format: FormatKind) {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let pkg = electron_like(src.path(), format);
        let (_, done) = install(root.path(), pkg).unwrap();
        let file = done.version_dir.join("opt/Foo/foo");
        assert!(file.is_file());
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(
            std::fs::read_link(done.version_dir.join("usr/bin/foo")).unwrap(),
            PathBuf::from("../../opt/Foo/foo")
        );
    }

    #[test]
    fn rpm_extracts() {
        assert_extracted(FormatKind::Rpm);
    }

    #[test]
    fn pacman_extracts() {
        assert_extracted(FormatKind::Pacman);
    }

    fn rpm_with(src: &Path, extra: &[E]) -> PathBuf {
        let elf = build_elf(&["libc.so.6"], None, &[], &[]);
        let mut data = vec![
            E::File("opt/Foo/foo", &elf, 0o755),
            E::Link("usr/bin/foo", "/opt/Foo/foo"),
        ];
        data.extend_from_slice(extra);
        build_rpm(
            src,
            "foo-1.2.3-1.pkg.rpm",
            RpmTags {
                name: "foo",
                version: "1.2.3",
                release: "1",
                arch: host_rpm_arch(),
                post: None,
                requires: &[],
                source: false,
            },
            &data,
        )
    }

    #[test]
    fn package_link_to_missing_target_is_skipped() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let rpm = rpm_with(
            src.path(),
            &[E::Link("usr/lib/x.so", "/usr/lib/libsystem.so")],
        );
        let (_, done) = install(root.path(), rpm).unwrap();
        assert!(done.version_dir.join("opt/Foo/foo").is_file());
        assert!(std::fs::symlink_metadata(done.version_dir.join("usr/lib/x.so")).is_err());
    }

    #[test]
    fn package_link_to_its_own_directory_points_at_dot() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let rpm = rpm_with(src.path(), &[E::Link("opt/Foo/self", "/opt/Foo")]);
        let (_, done) = install(root.path(), rpm).unwrap();
        assert_eq!(
            std::fs::read_link(done.version_dir.join("opt/Foo/self")).unwrap(),
            PathBuf::from(".")
        );
    }

    #[test]
    fn package_relative_link_leaving_the_tree_is_skipped() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let rpm = rpm_with(src.path(), &[E::Link("usr/lib/esc", "../../../../etc")]);
        let (_, done) = install(root.path(), rpm).unwrap();
        assert!(done.version_dir.join("opt/Foo/foo").is_file());
        assert!(std::fs::symlink_metadata(done.version_dir.join("usr/lib/esc")).is_err());
    }

    #[test]
    fn package_links_through_directory_links_resolve() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let rpm = rpm_with(
            src.path(),
            &[
                E::File("opt/foo/bin/tool", b"x", 0o755),
                E::Link("usr/lib/foo", "/opt/foo"),
                E::Link("usr/bin/tool", "/usr/lib/foo/bin/tool"),
                E::Link("usr/share/foo", "/opt/foo/bin"),
                E::Link("opt/foo/alias", "../foo/bin/tool"),
            ],
        );
        let (_, done) = install(root.path(), rpm).unwrap();
        let dir = &done.version_dir;
        assert_eq!(
            std::fs::read_link(dir.join("usr/bin/tool")).unwrap(),
            PathBuf::from("../../opt/foo/bin/tool")
        );
        assert_eq!(
            std::fs::read_link(dir.join("usr/lib/foo")).unwrap(),
            PathBuf::from("../../opt/foo")
        );
        for p in [
            "usr/bin/tool",
            "usr/lib/foo/bin/tool",
            "usr/share/foo/tool",
            "opt/foo/alias",
        ] {
            assert_eq!(std::fs::read(dir.join(p)).unwrap(), b"x", "{p}");
        }
    }

    #[test]
    fn cpio_traversal_refused() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let rpm = rpm_with(src.path(), &[E::File("../evil", b"x", 0o644)]);
        let artifact = unpakt_core::model::Artifact {
            file_name: "foo.rpm".into(),
            path: rpm,
        };
        let result = unpakt_core::format::detect(&artifact)
            .and_then(|f| f.inspect(&artifact))
            .map(|_| ());
        assert!(matches!(result, Err(Error::Refused(_))), "{result:?}");
        assert!(!root.path().join("evil").exists());
        assert!(!src.path().join("evil").exists());
    }
}

mod package_engine {
    use super::common::{E, MockForge, build_deb, build_elf, electron_like, host_package_arch};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use unpakt_core::compat::HostLibs;
    use unpakt_core::engine::{AddOpts, Engine};
    use unpakt_core::http::Http;
    use unpakt_core::model::FormatKind;
    use unpakt_core::{Layout, host::Host};

    fn engine(root: &Path, forge: Option<&MockForge>) -> Engine {
        let layout = Layout::under(root);
        let bin = OsString::from(layout.bin.clone());
        let libs = HostLibs::from_parts(
            BTreeMap::from([("libc.so.6".to_string(), PathBuf::from("/lib/libc.so.6"))]),
            BTreeMap::<String, BTreeSet<String>>::new(),
            true,
        );
        let engine = Engine::new(layout, Host::detect())
            .unwrap()
            .with_path_var(bin)
            .with_host_libs(libs);
        match forge {
            Some(f) => engine.with_http(Http::new(&f.api_url(), None).unwrap()),
            None => engine,
        }
    }

    fn exec_of(layout: &Layout, id: &str) -> String {
        let text =
            std::fs::read_to_string(layout.applications.join(format!("unpakt-{id}.desktop")))
                .unwrap();
        text.lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn package_installs_through_engine() {
        for format in [FormatKind::Deb, FormatKind::Rpm, FormatKind::Pacman] {
            let src = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let pkg = electron_like(src.path(), format);
            let mut engine = engine(root.path(), None);
            let layout = engine.layout().clone();

            let plan = engine
                .inspect(pkg.to_str().unwrap(), AddOpts::default())
                .unwrap();
            let id = engine.install(plan).unwrap().id;
            assert_eq!(id, "foo", "{format:?}");

            let current = layout.apps.join("foo").join("current");
            let entry = current.join("opt/Foo/foo");
            assert!(entry.is_file(), "{format:?}");
            assert_eq!(
                std::fs::metadata(&entry).unwrap().permissions().mode() & 0o111,
                0o111,
                "{format:?}"
            );
            let label = std::fs::read_link(&current).unwrap();
            let link = layout.apps.join("foo").join(&label).join("usr/bin/foo");
            assert!(
                std::fs::symlink_metadata(&link).unwrap().is_symlink(),
                "{format:?}"
            );
            assert_eq!(
                std::fs::read_link(&link).unwrap(),
                Path::new("../../opt/Foo/foo"),
                "{format:?}"
            );
            assert_eq!(
                std::fs::read_link(layout.bin.join("foo")).unwrap(),
                entry,
                "{format:?}"
            );
            assert_eq!(Path::new(&exec_of(&layout, "foo")), entry, "{format:?}");
        }
    }

    #[test]
    fn package_rollback_works() {
        let forge = MockForge::start();
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut engine = engine(root.path(), Some(&forge));
        let layout = engine.layout().clone();

        let publish = |version: &str| {
            let name = format!("foo_{version}_{}.deb", host_package_arch());
            let elf = build_elf(&["libc.so.6"], None, &[], &[]);
            let control = format!(
                "Package: foo\nVersion: {version}\nArchitecture: {}\nMaintainer: t <t@example.org>\n",
                host_package_arch()
            );
            let path = build_deb(
                src.path(),
                &name,
                &control,
                &[],
                &[E::File("opt/Foo/foo", &elf, 0o755)],
            );
            forge.publish(version, vec![(name, std::fs::read(path).unwrap())]);
        };

        publish("1.0.0");
        let plan = engine
            .inspect("https://github.com/o/r", AddOpts::default())
            .unwrap();
        let id = engine.install(plan).unwrap().id;
        publish("2.0.0");
        assert_eq!(engine.update(&id).unwrap(), Some("2.0.0".into()));
        let current = layout.apps.join(&id).join("current");
        assert_eq!(std::fs::read_link(&current).unwrap(), Path::new("2.0.0"));

        assert_eq!(engine.rollback(&id).unwrap(), "1.0.0");
        assert_eq!(std::fs::read_link(&current).unwrap(), Path::new("1.0.0"));
    }
}
