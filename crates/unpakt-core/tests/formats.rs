mod common;

use common::{Comp, E, build_appimage, build_tarball};
use std::path::Path;
use unpakt_core::Error;
use unpakt_core::format::detect;
use unpakt_core::model::{Artifact, ExecKind, FormatKind, Payload};

const ELF: &[u8] = &[
    0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x03, 0x00, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x00, 0x90, 0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0xa1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x38, 0x00, 0x0f, 0x00, 0x40, 0x00, 0x1d, 0x00, 0x1c, 0x00,
];
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

fn artifact(path: &Path) -> Artifact {
    Artifact {
        path: path.to_path_buf(),
        file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
    }
}

fn inspect(path: &Path) -> unpakt_core::Result<Payload> {
    let a = artifact(path);
    detect(&a)?.inspect(&a)
}

fn nvim_entries() -> Vec<E<'static>> {
    vec![
        E::File("nvim-linux-x86_64/bin/nvim", ELF, 0o755),
        E::File("nvim-linux-x86_64/lib/nvim/parser.so", ELF, 0o755),
        E::File(
            "nvim-linux-x86_64/share/applications/nvim.desktop",
            b"[Desktop Entry]\nType=Application\nName=Neovim\nIcon=nvim\nExec=nvim\n",
            0o644,
        ),
        E::File(
            "nvim-linux-x86_64/share/icons/hicolor/128x128/apps/nvim.png",
            PNG,
            0o644,
        ),
    ]
}

#[test]
fn detects_each_compression() {
    let tmp = tempfile::tempdir().unwrap();
    for (i, c) in [Comp::Gz, Comp::Xz, Comp::Zst, Comp::Bz2, Comp::None]
        .into_iter()
        .enumerate()
    {
        // Names deliberately lie about the compression.
        let path = build_tarball(tmp.path(), &format!("app-{i}.bin"), c, &nvim_entries());
        let a = artifact(&path);
        assert_eq!(detect(&a).unwrap().kind(), FormatKind::Tarball, "case {i}");
    }
}

#[test]
fn picks_entry_strips_prefix_finds_icon() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_tarball(
        tmp.path(),
        "nvim-linux-x86_64.tar.gz",
        Comp::Gz,
        &nvim_entries(),
    );
    let p = inspect(&path).unwrap();
    assert_eq!(p.format, FormatKind::Tarball);
    assert_eq!(p.name, "Neovim");
    assert_eq!(
        p.strip_prefix.as_deref(),
        Some(Path::new("nvim-linux-x86_64"))
    );
    assert_eq!(p.entries.len(), 1);
    assert_eq!(p.entries[0].path, Path::new("bin/nvim"));
    assert_eq!(p.entries[0].kind, ExecKind::Elf);
    assert!(p.desktop.unwrap().contains("Name=Neovim"));
    let icon = p.icon.unwrap();
    assert_eq!(icon.ext, "png");
    assert_eq!(icon.data, PNG);
}

#[test]
fn ranks_entries_and_reads_version() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_tarball(
        tmp.path(),
        "tool-1.2.3-linux-x86_64.tar.xz",
        Comp::Xz,
        &[
            E::File("tool-1.2.3/libexec/helper", ELF, 0o755),
            E::File("tool-1.2.3/bin/tool", ELF, 0o755),
            E::File("tool-1.2.3/run.sh", b"#!/bin/sh\n", 0o755),
            E::File("tool-1.2.3/README", b"hello", 0o644),
            E::File("tool-1.2.3/data.bin", ELF, 0o644),
        ],
    );
    let p = inspect(&path).unwrap();
    assert_eq!(p.name, "tool");
    assert_eq!(p.version.as_deref(), Some("1.2.3"));
    let order: Vec<_> = p.entries.iter().map(|e| e.path.clone()).collect();
    assert_eq!(
        order,
        [
            Path::new("bin/tool"),
            Path::new("run.sh"),
            Path::new("libexec/helper")
        ]
    );
    assert_eq!(p.entries[1].kind, ExecKind::Script);
    assert!(p.desktop.is_none() && p.icon.is_none());
}

#[test]
fn root_level_tarball_has_no_prefix_and_root_icon() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_tarball(
        tmp.path(),
        "foo.tar",
        Comp::None,
        &[
            E::File("foo", ELF, 0o755),
            E::File("foo.png", PNG, 0o644),
            E::Link("foo-link", "foo"),
        ],
    );
    let p = inspect(&path).unwrap();
    assert!(p.strip_prefix.is_none());
    assert_eq!(p.entries[0].path, Path::new("foo"));
    assert_eq!(p.icon.unwrap().ext, "png");
}

#[test]
fn refuses_source_tarball() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_tarball(
        tmp.path(),
        "proj-1.0.tar.gz",
        Comp::Gz,
        &[
            E::File("proj-1.0/configure", b"#!/bin/sh\n", 0o755),
            E::File("proj-1.0/src/main.c", b"int main(){}", 0o644),
            E::File("proj-1.0/build.sh", b"#!/bin/sh\n", 0o755),
        ],
    );
    assert!(matches!(inspect(&path), Err(Error::Refused(m)) if m.contains("source tarball")));
}

#[test]
fn refuses_tarball_without_executables() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_tarball(
        tmp.path(),
        "docs.tar",
        Comp::None,
        &[E::File("docs/readme.txt", b"hi", 0o644)],
    );
    assert!(matches!(inspect(&path), Err(Error::Refused(m)) if m.contains("no executable")));
}

#[test]
fn tarball_with_traversal_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let cases: Vec<Vec<E>> = vec![
        vec![E::File("../evil", ELF, 0o755)],
        vec![E::File("app/../../evil", ELF, 0o755)],
        vec![E::File("/etc/evil", ELF, 0o755)],
        vec![E::File("app/bin/app", ELF, 0o755), E::Link("app/x", "/etc")],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Link("app/sub/x", "../../.."),
        ],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Link("x", "../outside"),
        ],
        vec![E::File("./../x", ELF, 0o755)],
        vec![E::Link("a", "."), E::Link("a/l", "../x")],
        vec![E::Link("d/b", ".."), E::Link("a", "d/b/..")],
        vec![E::Link("a", "d/b/.."), E::Link("d/b", "..")],
        vec![E::File("app/bin/app", ELF, 0o755), E::Hard("app/h", "../x")],
        vec![
            E::File("app/bin/app", ELF, 0o755),
            E::Hard("app/h", "/etc/passwd"),
        ],
    ];
    for (i, entries) in cases.iter().enumerate() {
        let path = build_tarball(tmp.path(), &format!("bad-{i}.tar.gz"), Comp::Gz, entries);
        assert!(
            matches!(inspect(&path), Err(Error::Refused(ref m)) if m.contains("unsafe path")),
            "case {i}"
        );
    }
}

#[test]
fn links_inside_the_root_are_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_tarball(
        tmp.path(),
        "app.tar",
        Comp::None,
        &[
            E::File("app/bin/app", ELF, 0o755),
            E::Link("app/sub/up", "../bin/app"),
            E::Link("app/current", "."),
        ],
    );
    assert!(inspect(&path).is_ok());
}

#[test]
fn names_unsupported_formats() {
    let tmp = tempfile::tempdir().unwrap();
    let cases: [(&[u8], &str); 4] = [
        (b"!<arch>\ndebian-binary   ", "deb"),
        (b"\xED\xAB\xEE\xDB\x03\x00rest-of-rpm", "rpm"),
        (b"PK\x03\x04zipdata", "zip"),
        (ELF, "plain executable"),
    ];
    for (i, (bytes, word)) in cases.iter().enumerate() {
        let path = tmp.path().join(format!("f{i}.bin"));
        std::fs::write(&path, bytes).unwrap();
        let a = artifact(&path);
        match detect(&a) {
            Err(Error::Unsupported(m)) => assert!(m.contains(word), "{m} vs {word}"),
            other => panic!("case {i}: {:?}", other.map(|p| p.kind())),
        }
    }
    let path = tmp.path().join("junk");
    std::fs::write(&path, b"just some text").unwrap();
    match detect(&artifact(&path)) {
        Err(Error::Unsupported(m)) => assert!(m.contains("unknown format")),
        other => panic!("{:?}", other.map(|p| p.kind())),
    }
}

#[test]
fn appimage_desktop_and_symlinked_icon() {
    let d = tempfile::tempdir().unwrap();
    let path = build_appimage(
        d.path(),
        "Obsidian-1.6.7.AppImage",
        &[
            E::File(
                "app.desktop",
                b"[Desktop Entry]\nType=Application\nName=Obsidian\nIcon=obsidian\nExec=AppRun\n",
                0o644,
            ),
            E::File("obsidian.png", PNG, 0o644),
            E::Link(".DirIcon", "obsidian.png"),
            E::File("AppRun", b"#!/bin/sh\n", 0o755),
        ],
    );
    let p = inspect(&path).unwrap();
    assert_eq!(p.format, FormatKind::AppImage);
    assert_eq!(
        (p.name.as_str(), p.version.as_deref()),
        ("Obsidian", Some("1.6.7"))
    );
    assert!(p.desktop.unwrap().contains("Name=Obsidian"));
    let icon = p.icon.unwrap();
    assert_eq!(icon.ext, "png");
    assert_eq!(icon.data, PNG);
    assert!(p.warnings.is_empty());
}

#[test]
fn appimage_icon_symlink_leaving_image_is_dropped_with_warning() {
    let d = tempfile::tempdir().unwrap();
    let path = build_appimage(
        d.path(),
        "Tool-1.0.AppImage",
        &[
            E::File(
                "tool.desktop",
                b"[Desktop Entry]\nName=Tool\nIcon=tool\n",
                0o644,
            ),
            E::Link(".DirIcon", "../../etc/passwd"),
        ],
    );
    let p = inspect(&path).unwrap();
    assert!(p.icon.is_none());
    assert_eq!(p.warnings.len(), 1);
}

#[test]
fn appimage_version_prefers_desktop_key() {
    let d = tempfile::tempdir().unwrap();
    let path = build_appimage(
        d.path(),
        "Tool.AppImage",
        &[E::File(
            "tool.desktop",
            b"[Desktop Entry]\nName=Tool\nX-AppImage-Version=2.3.4\n",
            0o644,
        )],
    );
    let p = inspect(&path).unwrap();
    assert_eq!(p.version.as_deref(), Some("2.3.4"));
}

#[test]
fn type1_or_garbage_squashfs_degrades() {
    let d = tempfile::tempdir().unwrap();
    for (name, magic) in [
        ("Foo-1.2.AppImage", b"AI\x01"),
        ("Bar-3.4.AppImage", b"AI\x02"),
    ] {
        let mut bytes = ELF.to_vec();
        bytes.resize(4096, 0x5a);
        bytes[8..11].copy_from_slice(magic);
        let path = d.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        let p = inspect(&path).unwrap();
        assert_eq!(p.format, FormatKind::AppImage);
        assert_eq!(p.name, name.split('-').next().unwrap());
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].starts_with("could not read AppImage contents"));
    }
}
