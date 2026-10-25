mod common;

use common::{
    Comp, E, RpmTags, build_appimage, build_ar, build_deb, build_elf, build_pacman, build_rpm,
    build_rpm_with, build_tarball, electron_like_with, host_package_arch, host_rpm_arch,
};
use std::path::Path;
use unpakt_core::Error;
use unpakt_core::cancel::Cancel;
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
    detect(&a)?.inspect(&a, &Cancel::new())
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
    let cases: [(&[u8], &str); 2] = [(b"PK\x03\x04zipdata", "zip"), (ELF, "plain executable")];
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

#[test]
fn corrupt_data_block_degrades() {
    let d = tempfile::tempdir().unwrap();
    // Incompressible filler so the desktop file occupies real data blocks.
    let mut state = 0x9e3779b9u32;
    let filler: Vec<u8> = (0..60_000)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            b'a' + ((state >> 24) % 26) as u8
        })
        .collect();
    let mut desktop = b"[Desktop Entry]\nName=Tool\n#".to_vec();
    desktop.extend_from_slice(&filler);
    let path = build_appimage(
        d.path(),
        "Tool-1.0.AppImage",
        &[E::File("app.desktop", &desktop, 0o644)],
    );
    let mut bytes = std::fs::read(&path).unwrap();
    let start = unpakt_core::format::squashfs_offset(&bytes).unwrap() as usize + 96;
    for b in &mut bytes[start..start + 20_000] {
        *b = 0xff;
    }
    std::fs::write(&path, bytes).unwrap();

    let p = inspect(&path).unwrap();
    assert_eq!(p.name, "Tool");
    assert!(p.desktop.is_none() && p.icon.is_none());
    assert_eq!(p.warnings.len(), 1);
    assert!(p.warnings[0].starts_with("could not read AppImage contents"));
}

#[test]
fn oversized_tarball_icon_is_dropped_with_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let huge = vec![7u8; 9 * 1024 * 1024];
    let path = build_tarball(
        tmp.path(),
        "nvim-1.0.tar.gz",
        Comp::Gz,
        &[
            E::File("nvim-linux-x86_64/bin/nvim", ELF, 0o755),
            E::File(
                "nvim-linux-x86_64/share/applications/nvim.desktop",
                b"[Desktop Entry]\nType=Application\nName=Neovim\nIcon=nvim\nExec=nvim\n",
                0o644,
            ),
            E::File(
                "nvim-linux-x86_64/share/icons/hicolor/128x128/apps/nvim.png",
                &huge,
                0o644,
            ),
        ],
    );
    let p = inspect(&path).unwrap();
    assert!(p.icon.is_none());
    assert!(
        p.warnings
            .iter()
            .any(|w| w.contains("icon") && w.contains("too large")),
        "{:?}",
        p.warnings
    );
}

fn foreign_arch() -> &'static str {
    if host_package_arch() == "arm64" {
        "amd64"
    } else {
        "arm64"
    }
}

fn control(arch: &str) -> String {
    format!("Package: foo\nVersion: 1.2.3\nArchitecture: {arch}\n")
}

#[test]
fn deb_inspects_metadata_entry_desktop_icon() {
    let tmp = tempfile::tempdir().unwrap();
    let path = electron_like_with(
        tmp.path(),
        FormatKind::Deb,
        "Depends: libgtk-3-0, libnss3 (>= 3.26)\n",
    );
    let p = inspect(&path).unwrap();
    assert_eq!(p.format, FormatKind::Deb);
    assert_eq!(p.name, "Foo");
    assert_eq!(p.version.as_deref(), Some("1.2.3"));
    assert_eq!(p.entries[0].path, Path::new("opt/Foo/foo"));
    assert!(p.icon.is_some());
    let meta = p.package.unwrap();
    assert_eq!(meta.depends, ["libgtk-3-0", "libnss3 (>= 3.26)"]);
    assert_eq!(meta.scripts[0].name, "postinst");
    assert!(meta.scripts[0].text.contains("update-desktop-database"));
    assert_eq!(meta.index.setuid, [Path::new("opt/Foo/chrome-sandbox")]);
    assert!(meta.elf.contains_key(Path::new("opt/Foo/foo")));
    assert_eq!(meta.elf[Path::new("opt/Foo/foo")].needed, ["libc.so.6"]);
}

fn two_desktops_deb(dir: &Path, package: &str) -> std::path::PathBuf {
    let elf = build_elf(&[], None, &[], &[]);
    let url = b"[Desktop Entry]\nType=Application\nName=URL Handler\nNoDisplay=true\nExec=/usr/bin/code --open-url %U\n";
    let app = b"[Desktop Entry]\nType=Application\nName=Code\nExec=/usr/bin/code %F\n";
    let data = [
        E::File("usr/share/code/code", &elf, 0o755),
        E::Link("usr/bin/code", "/usr/share/code/code"),
        E::File(
            "usr/share/applications/code-url-handler.desktop",
            url,
            0o644,
        ),
        E::File("usr/share/applications/code.desktop", app, 0o644),
    ];
    let control = format!(
        "Package: {package}\nVersion: 1\nArchitecture: {}\n",
        host_package_arch()
    );
    build_deb(dir, "x.deb", &control, &[], &data)
}

#[test]
fn deb_desktop_prefers_the_package_named_file() {
    let tmp = tempfile::tempdir().unwrap();
    let p = inspect(&two_desktops_deb(tmp.path(), "code")).unwrap();
    assert_eq!(p.name, "Code");
}

#[test]
fn deb_desktop_skips_nodisplay_when_no_name_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let p = inspect(&two_desktops_deb(tmp.path(), "other")).unwrap();
    assert_eq!(p.name, "Code");
}

#[test]
fn deb_desktop_and_icon_reached_through_links() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let desktop =
        b"[Desktop Entry]\nType=Application\nName=Discord\nExec=/usr/bin/discord\nIcon=discord\n";
    // The links come first, so the targets are not known yet when they are read.
    let data = [
        E::Link(
            "usr/share/applications/discord.desktop",
            "../discord/discord.desktop",
        ),
        E::Link("usr/share/pixmaps/discord.png", "../discord/discord.png"),
        E::File("usr/bin/discord", &elf, 0o755),
        E::File("usr/share/discord/discord.desktop", desktop, 0o644),
        E::File(
            "usr/share/discord/discord.png",
            b"\x89PNG\r\n\x1a\nicon",
            0o644,
        ),
    ];
    let control = format!(
        "Package: discord\nVersion: 1\nArchitecture: {}\n",
        host_package_arch()
    );
    let path = build_deb(tmp.path(), "discord.deb", &control, &[], &data);
    let p = inspect(&path).unwrap();
    assert_eq!(p.name, "Discord");
    assert!(p.desktop.is_some());
    assert_eq!(p.icon.unwrap().data, b"\x89PNG\r\n\x1a\nicon");
}

#[test]
fn cancelled_package_inspect_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let path = electron_like_with(tmp.path(), FormatKind::Deb, "");
    let a = Artifact {
        file_name: "foo.deb".into(),
        path,
    };
    let cancel = Cancel::new();
    cancel.cancel();
    assert!(matches!(
        detect(&a).unwrap().inspect(&a, &cancel),
        Err(Error::Interrupted)
    ));
}

#[test]
fn deb_foreign_arch_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_deb(tmp.path(), "a.deb", &control(foreign_arch()), &[], &[]);
    assert!(matches!(inspect(&path), Err(Error::Refused(_))));
}

#[test]
fn deb_all_arch_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let data = [E::File("opt/foo/foo", &elf, 0o755)];
    let path = build_deb(tmp.path(), "a.deb", &control("all"), &[], &data);
    assert!(inspect(&path).is_ok());
}

#[test]
fn deb_with_bz2_data_is_unsupported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_ar(
        tmp.path(),
        "a.deb",
        &[
            ("debian-binary", b"2.0\n".to_vec()),
            ("data.tar.bz2", b"BZh".to_vec()),
        ],
    );
    assert!(matches!(inspect(&path), Err(Error::Unsupported(_))));
}

#[test]
fn deb_dot_paths_normalize() {
    let tmp = tempfile::tempdir().unwrap();
    let path = electron_like_with(tmp.path(), FormatKind::Deb, "");
    let meta = inspect(&path).unwrap().package.unwrap();
    assert!(!meta.index.files.is_empty());
    for p in meta.index.files.iter().chain(&meta.index.dirs) {
        assert!(!p.starts_with("."), "{}", p.display());
    }
    assert_eq!(
        meta.index.links[Path::new("usr/bin/foo")],
        Path::new("/opt/Foo/foo")
    );
}

#[test]
fn deb_without_desktop_falls_back_to_heuristic() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let data = [E::File("opt/foo/foo", &elf, 0o755)];
    let path = build_deb(tmp.path(), "a.deb", &control("all"), &[], &data);
    let p = inspect(&path).unwrap();
    assert_eq!(p.entries[0].path, Path::new("opt/foo/foo"));
    assert_eq!(p.name, "foo");
}

#[test]
fn deb_script_entry_keeps_text() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let data = [
        E::File("opt/Foo/foo", &elf, 0o755),
        E::File(
            "usr/bin/foo",
            b"#!/bin/sh\nexec /opt/Foo/foo \"$@\"\n",
            0o755,
        ),
        E::File(
            "usr/share/applications/foo.desktop",
            b"[Desktop Entry]\nType=Application\nName=Foo\nExec=/usr/bin/foo\n",
            0o644,
        ),
    ];
    let path = build_deb(tmp.path(), "a.deb", &control("all"), &[], &data);
    let p = inspect(&path).unwrap();
    assert_eq!(p.entries[0].path, Path::new("usr/bin/foo"));
    let meta = p.package.unwrap();
    assert!(meta.entry_script.unwrap().contains("exec /opt/Foo/foo"));
}

fn rpm_tags(arch: &str) -> RpmTags<'_> {
    RpmTags {
        name: "foo",
        version: "1.2.3",
        release: "1",
        arch,
        post: None,
        requires: &[],
        source: false,
    }
}

#[test]
fn rpm_inspects_metadata_and_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let path = electron_like_with(
        tmp.path(),
        FormatKind::Rpm,
        "rpmlib(CompressedFileNames)\n/bin/sh\nlibgtk-3.so.0()(64bit)",
    );
    let p = inspect(&path).unwrap();
    assert_eq!(p.format, FormatKind::Rpm);
    assert_eq!(p.version.as_deref(), Some("1.2.3-1"));
    assert_eq!(p.entries[0].path, Path::new("opt/Foo/foo"));
    let meta = p.package.unwrap();
    assert_eq!(meta.depends, ["libgtk-3.so.0()(64bit)"]);
    assert_eq!(meta.scripts[0].name, "%post");
    assert!(meta.scripts[0].text.contains("update-desktop-database"));
    assert_eq!(meta.index.setuid, [Path::new("opt/Foo/chrome-sandbox")]);
    assert!(meta.elf.contains_key(Path::new("opt/Foo/foo")));
}

#[test]
fn rpm_reads_transaction_scripts() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let data = [E::File("opt/foo/foo", &elf, 0o755)];
    let path = build_rpm_with(
        tmp.path(),
        "a.rpm",
        rpm_tags("noarch"),
        &data,
        &[(1151, "useradd evil"), (1152, "ldconfig")],
    );
    let meta = inspect(&path).unwrap().package.unwrap();
    let text = |name: &str| {
        meta.scripts
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.text.as_str())
    };
    assert_eq!(text("%pretrans"), Some("useradd evil"));
    assert_eq!(text("%posttrans"), Some("ldconfig"));
}

#[test]
fn rpm_reads_trigger_scripts() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let data = [E::File("opt/foo/foo", &elf, 0o755)];
    let path = build_rpm_with(
        tmp.path(),
        "a.rpm",
        rpm_tags("noarch"),
        &data,
        &[
            (1065, "useradd evil"),
            (5066, "ldconfig"),
            (5076, "systemctl daemon-reload"),
        ],
    );
    let meta = inspect(&path).unwrap().package.unwrap();
    let names: Vec<&str> = meta.scripts.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["%trigger", "%filetrigger", "%transfiletrigger"]);
    assert_eq!(meta.scripts[0].text, "useradd evil");
}

#[test]
fn rpm_noarch_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let data = [E::File("opt/foo/foo", &elf, 0o755)];
    let path = build_rpm(tmp.path(), "a.rpm", rpm_tags("noarch"), &data);
    assert!(inspect(&path).is_ok());
}

#[test]
fn rpm_aarch64_refused_on_x86_64() {
    if host_rpm_arch() != "x86_64" {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = build_rpm(tmp.path(), "a.rpm", rpm_tags("aarch64"), &[]);
    assert!(matches!(inspect(&path), Err(Error::Refused(_))));
}

#[test]
fn rpm_source_package_unsupported() {
    let tmp = tempfile::tempdir().unwrap();
    let mut tags = rpm_tags("x86_64");
    tags.source = true;
    let path = build_rpm(tmp.path(), "a.src.rpm", tags, &[]);
    assert!(matches!(inspect(&path), Err(Error::Unsupported(_))));
}

#[test]
fn rpm_truncated_header_is_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = electron_like_with(tmp.path(), FormatKind::Rpm, "");
    let bytes = std::fs::read(&path).unwrap();
    for cut in [10, 100, 150] {
        std::fs::write(&path, &bytes[..cut]).unwrap();
        assert!(matches!(inspect(&path), Err(Error::Parse(_))), "cut {cut}");
    }
}

#[test]
fn rpm_hard_linked_entry_keeps_elf_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&["libc.so.6"], None, &[], &[]);
    let data = [
        E::File("opt/foo/real", &elf, 0o755),
        E::Hard("opt/foo/foo", "opt/foo/real"),
    ];
    let path = build_rpm(tmp.path(), "a.rpm", rpm_tags("noarch"), &data);
    let p = inspect(&path).unwrap();
    let meta = p.package.unwrap();
    assert!(meta.elf.contains_key(Path::new("opt/foo/foo")));
    assert!(meta.elf.contains_key(Path::new("opt/foo/real")));
    assert!(meta.index.files.contains(Path::new("opt/foo/foo")));
}

#[test]
fn pacman_inspects_metadata_and_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let path = electron_like_with(
        tmp.path(),
        FormatKind::Pacman,
        "depend = gtk3\ndepend = nss\n",
    );
    let p = inspect(&path).unwrap();
    assert_eq!(p.format, FormatKind::Pacman);
    assert_eq!(p.name, "Foo");
    assert_eq!(p.entries[0].path, Path::new("opt/Foo/foo"));
    assert!(p.icon.is_some());
    let meta = p.package.unwrap();
    assert_eq!(meta.depends, ["gtk3", "nss"]);
    assert_eq!(meta.scripts[0].name, ".INSTALL");
    assert!(meta.scripts[0].text.contains("update-desktop-database"));
    assert_eq!(meta.index.setuid, [Path::new("opt/Foo/chrome-sandbox")]);
    assert!(!meta.index.files.contains(Path::new(".PKGINFO")));
    assert!(!meta.index.files.contains(Path::new(".INSTALL")));
}

#[test]
fn pacman_any_arch_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = build_elf(&[], None, &[], &[]);
    let data = [E::File("opt/foo/foo", &elf, 0o755)];
    let path = build_pacman(
        tmp.path(),
        "a.pkg.tar.zst",
        "pkgname = foo\npkgver = 1-1\narch = any\n",
        None,
        &data,
        Comp::Zst,
    );
    assert!(inspect(&path).is_ok());
}

#[test]
fn pacman_foreign_arch_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let foreign = if host_package_arch() == "arm64" {
        "x86_64"
    } else {
        "aarch64"
    };
    let info = format!("pkgname = foo\npkgver = 1-1\narch = {foreign}\n");
    let path = build_pacman(tmp.path(), "a.pkg.tar.zst", &info, None, &[], Comp::Zst);
    assert!(matches!(inspect(&path), Err(Error::Refused(_))));
}

#[test]
fn pacman_xz_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let info = "pkgname = foo\npkgver = 1-1\narch = any\n";
    let path = build_pacman(tmp.path(), "a.pkg.tar.xz", info, None, &[], Comp::Xz);
    assert_eq!(detect(&artifact(&path)).unwrap().kind(), FormatKind::Pacman);
}

#[test]
fn tar_zst_without_pkginfo_is_tarball() {
    let tmp = tempfile::tempdir().unwrap();
    let data = [E::File("foo/foo", ELF, 0o755)];
    let path = build_tarball(tmp.path(), "foo.pkg.tar.zst", Comp::Zst, &data);
    assert_eq!(
        detect(&artifact(&path)).unwrap().kind(),
        FormatKind::Tarball
    );
}

#[test]
fn tarball_with_late_pkginfo_is_tarball() {
    let tmp = tempfile::tempdir().unwrap();
    let data = [
        E::File("usr/x", ELF, 0o755),
        E::File(".PKGINFO", b"pkgname = foo\n", 0o644),
    ];
    let path = build_tarball(tmp.path(), "foo.tar.zst", Comp::Zst, &data);
    assert_eq!(
        detect(&artifact(&path)).unwrap().kind(),
        FormatKind::Tarball
    );
}

#[test]
fn nested_pkginfo_is_not_pacman() {
    let tmp = tempfile::tempdir().unwrap();
    let data = [E::File("x/.PKGINFO", b"pkgname = foo\n", 0o644)];
    let path = build_tarball(tmp.path(), "foo.tar.zst", Comp::Zst, &data);
    assert_eq!(
        detect(&artifact(&path)).unwrap().kind(),
        FormatKind::Tarball
    );
}

#[test]
fn pacman_dot_prefixed_pkginfo_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let data = [E::File("./.PKGINFO", b"pkgname = foo\n", 0o644)];
    let path = build_tarball(tmp.path(), "a.pkg.tar.zst", Comp::Zst, &data);
    assert_eq!(detect(&artifact(&path)).unwrap().kind(), FormatKind::Pacman);
}

#[test]
fn pacman_missing_fields_is_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = build_pacman(
        tmp.path(),
        "a.pkg.tar.zst",
        "pkgname = foo\n",
        None,
        &[],
        Comp::Zst,
    );
    assert!(matches!(inspect(&path), Err(Error::Parse(_))));
}

#[test]
fn pacman_oversized_pkginfo_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let big = format!("pkgname = foo\n{}", "#".repeat(2 * 1024 * 1024));
    let path = build_pacman(tmp.path(), "a.pkg.tar.zst", &big, None, &[], Comp::Zst);
    assert!(matches!(inspect(&path), Err(Error::Refused(_))));
}
