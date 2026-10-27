#![allow(dead_code)]

pub mod fake_pk;

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Maps every path under `root` (relative) to `dir`, `link:<target>` or
/// `file:<mode>:<sha256>`. Symlinks are never followed. Database files, the
/// cache dir and the desktop mime cache are ignored.
pub fn tree_snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn ignored(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    const DB: [&str; 6] = [
        "share/unpakt/lock",
        "share/unpakt/journal",
        "share/unpakt/db.sqlite",
        "share/unpakt/db.sqlite-wal",
        "share/unpakt/db.sqlite-shm",
        "share/unpakt/db.sqlite-journal",
    ];
    DB.iter().any(|d| s.ends_with(d))
        || s == ".cache/unpakt"
        || s.starts_with(".cache/unpakt/")
        || s.ends_with("applications/mimeinfo.cache")
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, String>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let rel = path.strip_prefix(root).unwrap().to_path_buf();
        if ignored(&rel) {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let ty = meta.file_type();
        if ty.is_symlink() {
            let target = std::fs::read_link(&path).unwrap();
            out.insert(rel, format!("link:{}", target.display()));
        } else if ty.is_dir() {
            out.insert(rel, "dir".to_string());
            walk(root, &path, out);
        } else {
            let bytes = std::fs::read(&path).unwrap();
            let sum = hex::encode(Sha256::digest(&bytes));
            out.insert(
                rel,
                format!("file:{:o}:{sum}", meta.permissions().mode() & 0o7777),
            );
        }
    }
}

pub enum Comp {
    Gz,
    Xz,
    Zst,
    Bz2,
    None,
}

#[derive(Clone)]
pub enum E<'a> {
    /// Path, contents, mode.
    File(&'a str, &'a [u8], u32),
    /// Path, link target.
    Link(&'a str, &'a str),
    /// Path, hardlink target.
    Hard(&'a str, &'a str),
}

/// Writes a tarball named `name` under `dir`. Entry names are stored verbatim,
/// so hostile paths (`../x`, absolute) end up in the archive as written.
pub fn build_tarball(dir: &Path, name: &str, comp: Comp, entries: &[E]) -> PathBuf {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        let (path, data): (&str, &[u8]) = match entry {
            E::Hard(path, target) => {
                header.set_entry_type(tar::EntryType::Link);
                header.set_mode(0o644);
                header.as_old_mut().linkname[..target.len()].copy_from_slice(target.as_bytes());
                (path, &[])
            }
            E::File(path, data, mode) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                (path, data)
            }
            E::Link(path, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_link_name(target).unwrap();
                (path, &[])
            }
        };
        let raw = path.as_bytes();
        assert!(raw.len() < 100, "fixture path too long: {path}");
        header.as_old_mut().name[..raw.len()].copy_from_slice(raw);
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }
    let tar_bytes = builder.into_inner().unwrap();
    let bytes = match comp {
        Comp::None => tar_bytes,
        Comp::Gz => {
            let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            w.write_all(&tar_bytes).unwrap();
            w.finish().unwrap()
        }
        Comp::Xz => {
            let mut w = liblzma::write::XzEncoder::new(Vec::new(), 1);
            w.write_all(&tar_bytes).unwrap();
            w.finish().unwrap()
        }
        Comp::Zst => zstd::stream::encode_all(&tar_bytes[..], 1).unwrap(),
        Comp::Bz2 => {
            let mut w = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
            w.write_all(&tar_bytes).unwrap();
            w.finish().unwrap()
        }
    };
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Writes an `ar` archive named `name` under `dir` holding `members` in order.
pub fn build_ar(dir: &Path, name: &str, members: &[(&str, Vec<u8>)]) -> PathBuf {
    let path = dir.join(name);
    let mut builder = ar::Builder::new(std::fs::File::create(&path).unwrap());
    for (member, data) in members {
        builder
            .append(
                &ar::Header::new(member.as_bytes().to_vec(), data.len() as u64),
                &data[..],
            )
            .unwrap();
    }
    path
}

fn gz(bytes: Vec<u8>) -> Vec<u8> {
    let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    w.write_all(&bytes).unwrap();
    w.finish().unwrap()
}

/// A tar whose paths carry a `./` prefix after a leading `./` directory entry.
fn dotted_tar(entries: &[E]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    builder.append_data(&mut dir, "./", &b""[..]).unwrap();
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        match entry {
            E::File(path, data, mode) => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(data.len() as u64);
                builder
                    .append_data(&mut header, format!("./{path}"), *data)
                    .unwrap();
            }
            E::Link(path, target) | E::Hard(path, target) => {
                let hard = matches!(entry, E::Hard(..));
                header.set_entry_type(if hard {
                    tar::EntryType::Link
                } else {
                    tar::EntryType::Symlink
                });
                header.set_mode(0o777);
                header.set_size(0);
                builder
                    .append_link(&mut header, format!("./{path}"), target)
                    .unwrap();
            }
        }
    }
    builder.into_inner().unwrap()
}

/// Writes a deb named `name` under `dir`: `debian-binary`, `control.tar.gz`
/// (the `control` text plus `scripts`) and `data.tar.gz`.
pub fn build_deb(
    dir: &Path,
    name: &str,
    control: &str,
    scripts: &[(&str, &[u8])],
    data: &[E],
) -> PathBuf {
    let mut control_entries = vec![E::File("control", control.as_bytes(), 0o644)];
    for (script, text) in scripts {
        control_entries.push(E::File(script, text, 0o755));
    }
    build_ar(
        dir,
        name,
        &[
            ("debian-binary", b"2.0\n".to_vec()),
            ("control.tar.gz", gz(dotted_tar(&control_entries))),
            ("data.tar.gz", gz(dotted_tar(data))),
        ],
    )
}

/// The package architecture string that matches this machine.
pub fn host_package_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => "all",
    }
}

/// The rpm architecture string that matches this machine.
pub fn host_rpm_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => "noarch",
    }
}

pub struct RpmTags<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub release: &'a str,
    pub arch: &'a str,
    pub post: Option<&'a str>,
    pub requires: &'a [&'a str],
    pub source: bool,
}

fn rpm_header(tags: &[(u32, u32, Vec<u8>, u32)], signature: bool) -> Vec<u8> {
    let mut index = Vec::new();
    let mut store = Vec::new();
    for (tag, kind, bytes, count) in tags {
        if *kind == 4 {
            while store.len() % 4 != 0 {
                store.push(0);
            }
        }
        index.extend_from_slice(&tag.to_be_bytes());
        index.extend_from_slice(&kind.to_be_bytes());
        index.extend_from_slice(&(store.len() as u32).to_be_bytes());
        index.extend_from_slice(&count.to_be_bytes());
        store.extend_from_slice(bytes);
    }
    let mut out = vec![0x8e, 0xad, 0xe8, 0x01, 0, 0, 0, 0];
    out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
    out.extend_from_slice(&(store.len() as u32).to_be_bytes());
    out.extend(index);
    out.extend(store);
    while signature && out.len() % 8 != 0 {
        out.push(0);
    }
    out
}

/// A `newc` archive with `./`-prefixed paths. Hard links are written ahead of
/// the record that carries their data, the way rpm lays them out.
fn rpm_cpio(entries: &[E]) -> Vec<u8> {
    fn record(ino: u32, mode: u32, nlink: u32, name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "070701{ino:08X}{mode:08X}{:08X}{:08X}{nlink:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}",
            0, 0, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0
        )
        .into_bytes();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }
    let mut out = Vec::new();
    let mut ino = 0;
    for entry in entries {
        ino += 1;
        match entry {
            E::File(path, data, mode) => {
                let links: Vec<&str> = entries
                    .iter()
                    .filter_map(|e| match e {
                        E::Hard(p, t) if t == path => Some(*p),
                        _ => None,
                    })
                    .collect();
                let nlink = 1 + links.len() as u32;
                for link in links {
                    out.extend(record(
                        ino,
                        0o100000 | mode,
                        nlink,
                        &format!("./{link}"),
                        b"",
                    ));
                }
                out.extend(record(
                    ino,
                    0o100000 | mode,
                    nlink,
                    &format!("./{path}"),
                    data,
                ));
            }
            E::Link(path, target) => {
                out.extend(record(
                    ino,
                    0o120777,
                    1,
                    &format!("./{path}"),
                    target.as_bytes(),
                ));
            }
            E::Hard(..) => {}
        }
    }
    out.extend(record(0, 0, 1, "TRAILER!!!", b""));
    out
}

/// Writes an rpm named `name` under `dir`: lead, an empty signature header,
/// the main header and a gzip cpio payload.
pub fn build_rpm(dir: &Path, name: &str, t: RpmTags, data: &[E]) -> PathBuf {
    build_rpm_with(dir, name, t, data, &[])
}

/// Like [`build_rpm`], with extra string tags `(tag, value)` in the main header.
pub fn build_rpm_with(
    dir: &Path,
    name: &str,
    t: RpmTags,
    data: &[E],
    extra: &[(u32, &str)],
) -> PathBuf {
    build_rpm_full(dir, name, t, data, extra, &[])
}

/// Like [`build_rpm_with`], with INT32 tags `(tag, value, declared count)`.
/// Each tag stores one value; a count above 1 makes the entry run past the
/// end of the store when it is the last one.
pub fn build_rpm_full(
    dir: &Path,
    name: &str,
    t: RpmTags,
    data: &[E],
    extra: &[(u32, &str)],
    ints: &[(u32, u32, u32)],
) -> PathBuf {
    let string = |tag: u32, v: &str| (tag, 6, [v.as_bytes(), &[0]].concat(), 1);
    let mut tags = vec![
        string(1000, t.name),
        string(1001, t.version),
        string(1002, t.release),
        string(1022, t.arch),
    ];
    if let Some(post) = t.post {
        tags.push(string(1024, post));
    }
    for (tag, value) in extra {
        tags.push(string(*tag, value));
    }
    for (tag, value, count) in ints {
        tags.push((*tag, 4, value.to_be_bytes().to_vec(), *count));
    }
    if !t.source {
        tags.push(string(1044, "foo-1.2.3-1.src.rpm"));
    }
    let requires: Vec<u8> = t
        .requires
        .iter()
        .flat_map(|r| r.bytes().chain([0]))
        .collect();
    if !t.requires.is_empty() {
        tags.push((1049, 8, requires, t.requires.len() as u32));
    }
    let mut lead = vec![
        0xed,
        0xab,
        0xee,
        0xdb,
        3,
        0,
        0,
        if t.source { 1 } else { 0 },
    ];
    lead.resize(96, 0);
    let mut bytes = lead;
    bytes.extend(rpm_header(&[], true));
    bytes.extend(rpm_header(&tags, false));
    bytes.extend(gz(rpm_cpio(data)));
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Writes a pacman package named `name` under `dir`: `.PKGINFO` first, then
/// the optional `.INSTALL`, then `data`.
pub fn build_pacman(
    dir: &Path,
    name: &str,
    pkginfo: &str,
    install: Option<&str>,
    data: &[E],
    comp: Comp,
) -> PathBuf {
    let mut entries = vec![E::File(".PKGINFO", pkginfo.as_bytes(), 0o644)];
    if let Some(text) = install {
        entries.push(E::File(".INSTALL", text.as_bytes(), 0o644));
    }
    for e in data {
        entries.push(match e {
            E::File(p, d, m) => E::File(p, d, *m),
            E::Link(p, t) => E::Link(p, t),
            E::Hard(p, t) => E::Hard(p, t),
        });
    }
    build_tarball(dir, name, comp, &entries)
}

/// An Electron-style app packaged as `format`: a launcher link, a setuid
/// helper, a desktop file, an icon and a postinst script. Version `1.2.3`.
pub fn electron_like(dir: &Path, format: unpakt_core::model::FormatKind) -> PathBuf {
    electron_like_with(dir, format, "")
}

/// Like [`electron_like`], with `extra_control` appended to the control fields
/// (for an rpm, one required name per line).
pub fn electron_like_with(
    dir: &Path,
    format: unpakt_core::model::FormatKind,
    extra_control: &str,
) -> PathBuf {
    let elf = build_elf(&["libc.so.6"], None, &[], &[]);
    let desktop = b"[Desktop Entry]\nType=Application\nName=Foo\nExec=/usr/bin/foo %U\nIcon=foo\n";
    let png = b"\x89PNG\r\n\x1a\nfake";
    let data = [
        E::File("opt/Foo/foo", &elf, 0o755),
        E::File("opt/Foo/chrome-sandbox", b"sandbox", 0o4755),
        E::Link("usr/bin/foo", "/opt/Foo/foo"),
        E::File("usr/share/applications/foo.desktop", desktop, 0o644),
        E::File("usr/share/icons/hicolor/256x256/apps/foo.png", png, 0o644),
    ];
    let postinst: &[u8] = b"#!/bin/sh\nset -e\nupdate-desktop-database -q || true\n";
    match format {
        unpakt_core::model::FormatKind::Deb => {
            let control = format!(
                "Package: foo\nVersion: 1.2.3\nArchitecture: {}\nMaintainer: t <t@example.org>\n{extra_control}",
                host_package_arch()
            );
            build_deb(
                dir,
                "foo_1.2.3_pkg.deb",
                &control,
                &[("postinst", postinst)],
                &data,
            )
        }
        unpakt_core::model::FormatKind::Rpm => {
            let requires: Vec<&str> = extra_control.lines().collect();
            build_rpm(
                dir,
                "foo-1.2.3-1.pkg.rpm",
                RpmTags {
                    name: "foo",
                    version: "1.2.3",
                    release: "1",
                    arch: host_rpm_arch(),
                    post: Some(std::str::from_utf8(postinst).unwrap()),
                    requires: &requires,
                    source: false,
                },
                &data,
            )
        }
        unpakt_core::model::FormatKind::Pacman => {
            let pkginfo = format!(
                "pkgname = foo\npkgver = 1.2.3-1\narch = {}\n{extra_control}",
                host_rpm_arch()
            );
            build_pacman(
                dir,
                "foo-1.2.3-1-pkg.pkg.tar.zst",
                &pkginfo,
                Some(std::str::from_utf8(postinst).unwrap()),
                &data,
                Comp::Zst,
            )
        }
        other => unimplemented!("electron_like for {other:?}"),
    }
}

/// A shell script that creates `marker` when run.
pub fn marker_script(marker: &Path) -> Vec<u8> {
    format!("#!/bin/sh\ntouch '{}'\n", marker.display()).into_bytes()
}

/// Writes a type 2 AppImage named `name` under `dir`: a real ELF header, the
/// `AI\x02` magic, and a gzip squashfs holding `files` right after the ELF.
/// Only `E::File` and `E::Link` entries are supported.
pub fn build_appimage(dir: &Path, name: &str, files: &[E]) -> PathBuf {
    build_appimage_with(dir, name, files, backhand::compression::Compressor::Gzip)
}

/// Test-only squashfs writer codec: every block is zstd-compressed.
struct ZstdWriter;

static ZSTD_WRITER: ZstdWriter = ZstdWriter;

impl backhand::compression::CompressionAction for ZstdWriter {
    type Error = backhand::BackhandError;
    type Compressor = backhand::compression::Compressor;
    type FilesystemCompressor = backhand::FilesystemCompressor;
    type SuperBlock = backhand::SuperBlock;

    fn decompress(
        &self,
        bytes: &[u8],
        out: &mut Vec<u8>,
        _: Self::Compressor,
    ) -> Result<(), Self::Error> {
        out.extend_from_slice(&zstd::decode_all(bytes).map_err(backhand::BackhandError::StdIo)?);
        Ok(())
    }

    fn compress(
        &self,
        bytes: &[u8],
        _: Self::FilesystemCompressor,
        _: u32,
    ) -> Result<Vec<u8>, Self::Error> {
        zstd::bulk::compress(bytes, 3).map_err(backhand::BackhandError::StdIo)
    }
}

/// Like [`build_appimage`], with the squashfs compressed by `compressor`.
pub fn build_appimage_with(
    dir: &Path,
    name: &str,
    files: &[E],
    compressor: backhand::compression::Compressor,
) -> PathBuf {
    use backhand::{FilesystemWriter, NodeHeader};

    let mut image = std::fs::read("/usr/bin/true").unwrap();
    let offset = unpakt_core::format::squashfs_offset(&image).unwrap() as usize;
    image.resize(offset, 0);
    image[8..11].copy_from_slice(b"AI\x02");

    let mut writer = FilesystemWriter::default();
    if compressor == backhand::compression::Compressor::Zstd {
        // backhand's own zstd is off (RustSec), so this fixture compresses blocks itself.
        writer.set_kind(backhand::kind::Kind::new_v4(&ZSTD_WRITER));
    }
    writer.set_compressor(backhand::FilesystemCompressor::new(compressor, None).unwrap());
    for entry in files {
        match entry {
            E::File(path, data, mode) => writer
                .push_file(
                    std::io::Cursor::new(data.to_vec()),
                    path,
                    NodeHeader::new(*mode as u16, 0, 0, 0),
                )
                .unwrap(),
            E::Link(path, target) => writer
                .push_symlink(target, path, NodeHeader::new(0o777, 0, 0, 0))
                .unwrap(),
            E::Hard(..) => panic!("hard links are not supported in AppImage fixtures"),
        }
    }
    let mut squashfs = std::io::Cursor::new(Vec::new());
    writer.write(&mut squashfs).unwrap();
    image.extend_from_slice(&squashfs.into_inner());

    let path = dir.join(name);
    std::fs::write(&path, image).unwrap();
    path
}

#[derive(Default)]
struct ForgeState {
    tag: String,
    assets: Vec<(String, Vec<u8>)>,
    truncate: bool,
    /// When set, HEAD requests are answered with this status and no headers.
    head_status: Option<u16>,
    /// Path and `Authorization` header of every request received.
    seen: Vec<(String, Option<String>)>,
}

/// An in-process stand-in for a release API and its download host. It serves
/// `/repos/o/r/releases/latest` and `/dl/<name>` on a loopback port.
pub struct MockForge {
    port: u16,
    state: std::sync::Arc<std::sync::Mutex<ForgeState>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockForge {
    pub fn start() -> MockForge {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(ForgeState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = Arc::clone(&state);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        let _ = serve(stream, port, &state);
                    }
                }
            })
        };
        MockForge {
            port,
            state,
            stop,
            thread: Some(thread),
        }
    }

    pub fn api_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn download_url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/dl/{name}", self.port)
    }

    pub fn publish(&self, tag: &str, assets: Vec<(String, Vec<u8>)>) {
        let mut state = self.state.lock().unwrap();
        state.tag = tag.to_string();
        state.assets = assets;
    }

    pub fn set_truncate(&self, on: bool) {
        self.state.lock().unwrap().truncate = on;
    }

    pub fn set_head_status(&self, status: u16) {
        self.state.lock().unwrap().head_status = Some(status);
    }

    /// The quoted ETag served for the asset `name`.
    pub fn etag(&self, name: &str) -> String {
        let state = self.state.lock().unwrap();
        let (_, bytes) = state.assets.iter().find(|(n, _)| n == name).unwrap();
        etag_of(bytes)
    }

    /// How many requests for exactly `path` have been received.
    pub fn request_count(&self, path: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .seen
            .iter()
            .filter(|(p, _)| p == path)
            .count()
    }

    /// The `Authorization` headers received for requests to `path`.
    pub fn auth_for(&self, path: &str) -> Vec<Option<String>> {
        let state = self.state.lock().unwrap();
        state
            .seen
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, a)| a.clone())
            .collect()
    }
}

impl Drop for MockForge {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn etag_of(bytes: &[u8]) -> String {
    format!("\"{}\"", &hex::encode(Sha256::digest(bytes))[..16])
}

fn serve(
    mut stream: std::net::TcpStream,
    port: u16,
    state: &std::sync::Mutex<ForgeState>,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader};

    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut auth = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            auth = Some(value.trim().to_string());
        }
    }

    let mut state = state.lock().unwrap();
    state.seen.push((path.clone(), auth));
    let head = method == "HEAD";

    let (status, extra, body, claimed_len): (&str, String, Vec<u8>, usize) =
        if let (true, Some(code)) = (head, state.head_status) {
            let status = match code {
                405 => "405 Method Not Allowed",
                _ => "500 Internal Server Error",
            };
            (status, String::new(), Vec::new(), 0)
        } else if path == "/repos/o/r/releases/latest" {
            let assets: Vec<serde_json::Value> = state
                .assets
                .iter()
                .map(|(name, bytes)| {
                    serde_json::json!({
                        "name": name,
                        "browser_download_url": format!("http://127.0.0.1:{port}/dl/{name}"),
                        "size": bytes.len(),
                    })
                })
                .collect();
            let body =
                serde_json::to_vec(&serde_json::json!({"tag_name": state.tag, "assets": assets}))
                    .unwrap();
            let len = body.len();
            (
                "200 OK",
                "Content-Type: application/json\r\n".into(),
                body,
                len,
            )
        } else if let Some(name) = path.strip_prefix("/dl/")
            && let Some((_, bytes)) = state.assets.iter().find(|(n, _)| n == name)
        {
            let claimed = if state.truncate {
                bytes.len() + 1024
            } else {
                bytes.len()
            };
            (
                "200 OK",
                format!(
                    "ETag: {}\r\nLast-Modified: Sat, 01 Jan 2000 00:00:00 GMT\r\n",
                    etag_of(bytes)
                ),
                bytes.clone(),
                claimed,
            )
        } else {
            ("404 Not Found", String::new(), Vec::new(), 0)
        };
    drop(state);

    write!(
        stream,
        "HTTP/1.1 {status}\r\n{extra}Content-Length: {claimed_len}\r\nConnection: close\r\n\r\n"
    )?;
    if !head {
        stream.write_all(&body)?;
    }
    stream.flush()?;
    Ok(())
}

/// A `newc` cpio archive of `(name, mode, data or link target)` entries,
/// ended by a trailer.
pub fn newc(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    fn record(ino: u32, mode: u32, name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "070701{ino:08X}{mode:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}",
            0, 0, 1, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0
        )
        .into_bytes();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }
    let mut out = Vec::new();
    for (i, (name, mode, data)) in entries.iter().enumerate() {
        out.extend(record(i as u32 + 1, *mode, name, data));
    }
    out.extend(record(0, 0, "TRAILER!!!", b""));
    out
}

/// Builds a minimal x86_64 `ET_DYN` ELF with a dynamic section, string table,
/// symbol-version needs and a read-only data blob holding `strings`.
pub fn build_elf(
    needed: &[&str],
    runpath: Option<&str>,
    version_needs: &[(&str, &str)],
    strings: &[&str],
) -> Vec<u8> {
    fn put(buf: &mut Vec<u8>, s: &str) -> u32 {
        let off = buf.len() as u32;
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
        off
    }
    fn elf_hash(name: &str) -> u32 {
        let mut h: u32 = 0;
        for &b in name.as_bytes() {
            h = (h << 4).wrapping_add(u32::from(b));
            let g = h & 0xf000_0000;
            if g != 0 {
                h ^= g >> 24;
            }
            h &= !g;
        }
        h
    }

    let mut dynstr = vec![0u8];
    let needed_offs: Vec<u32> = needed.iter().map(|n| put(&mut dynstr, n)).collect();
    let runpath_off = runpath.map(|r| put(&mut dynstr, r));

    let mut sonames: Vec<&str> = Vec::new();
    for (so, _) in version_needs {
        if !sonames.contains(so) {
            sonames.push(so);
        }
    }
    let mut verneed = Vec::new();
    for (i, so) in sonames.iter().enumerate() {
        let file_off = put(&mut dynstr, so);
        let auxes: Vec<&str> = version_needs
            .iter()
            .filter(|(s, _)| s == so)
            .map(|(_, v)| *v)
            .collect();
        let last = i + 1 == sonames.len();
        let next = if last {
            0
        } else {
            16 + 16 * auxes.len() as u32
        };
        verneed.extend_from_slice(&1u16.to_le_bytes());
        verneed.extend_from_slice(&(auxes.len() as u16).to_le_bytes());
        verneed.extend_from_slice(&file_off.to_le_bytes());
        verneed.extend_from_slice(&16u32.to_le_bytes());
        verneed.extend_from_slice(&next.to_le_bytes());
        for (j, v) in auxes.iter().enumerate() {
            let name_off = put(&mut dynstr, v);
            let aux_next = if j + 1 == auxes.len() { 0 } else { 16u32 };
            verneed.extend_from_slice(&elf_hash(v).to_le_bytes());
            verneed.extend_from_slice(&0u16.to_le_bytes());
            verneed.extend_from_slice(&(2 + j as u16).to_le_bytes());
            verneed.extend_from_slice(&name_off.to_le_bytes());
            verneed.extend_from_slice(&aux_next.to_le_bytes());
        }
    }

    let mut rodata = Vec::new();
    for s in strings {
        put(&mut rodata, s);
    }

    let mut entries: Vec<(u64, u64)> = Vec::new();
    for off in &needed_offs {
        entries.push((1, u64::from(*off)));
    }
    if let Some(off) = runpath_off {
        entries.push((29, u64::from(off)));
    }

    const EHDR: u64 = 64;
    const PHDR: u64 = 56;
    let dynstr_at = EHDR + 2 * PHDR;
    let verneed_at = (dynstr_at + dynstr.len() as u64).next_multiple_of(8);
    let dynamic_at = (verneed_at + verneed.len() as u64).next_multiple_of(8);
    let n_entries = entries.len() as u64 + 3 + if verneed.is_empty() { 0 } else { 2 } + 1;
    let dynamic_len = n_entries * 16;
    let rodata_at = dynamic_at + dynamic_len;
    let shoff = (rodata_at + rodata.len() as u64).next_multiple_of(8);
    let total = shoff + 3 * 64;

    entries.push((5, dynstr_at));
    entries.push((10, dynstr.len() as u64));
    entries.push((0x6fff_fffe, verneed_at));
    entries.push((0x6fff_ffff, sonames.len() as u64));
    if verneed.is_empty() {
        entries.retain(|(t, _)| *t != 0x6fff_fffe && *t != 0x6fff_ffff);
    }
    entries.push((0, 0));
    while (entries.len() as u64) < n_entries {
        entries.insert(entries.len() - 1, (0x7fff_ffff, 0));
    }

    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&62u16.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&EHDR.to_le_bytes());
    out.extend_from_slice(&shoff.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(EHDR as u16).to_le_bytes());
    out.extend_from_slice(&(PHDR as u16).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&64u16.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    let phdr = |out: &mut Vec<u8>, ty: u32, flags: u32, off: u64, len: u64, align: u64| {
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&align.to_le_bytes());
    };
    phdr(&mut out, 1, 5, 0, total, 1);
    phdr(&mut out, 2, 6, dynamic_at, dynamic_len, 8);

    out.extend_from_slice(&dynstr);
    out.resize(verneed_at as usize, 0);
    out.extend_from_slice(&verneed);
    out.resize(dynamic_at as usize, 0);
    for (tag, val) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    out.extend_from_slice(&rodata);
    out.resize(shoff as usize, 0);

    let shdr = |out: &mut Vec<u8>, ty: u32, off: u64, len: u64, link: u32, info: u32| {
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&2u64.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&link.to_le_bytes());
        out.extend_from_slice(&info.to_le_bytes());
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
    };
    out.extend_from_slice(&[0; 64]);
    shdr(&mut out, 3, dynstr_at, dynstr.len() as u64, 0, 0);
    shdr(
        &mut out,
        0x6fff_fffe,
        verneed_at,
        verneed.len() as u64,
        1,
        sonames.len() as u32,
    );
    out
}

/// A deb of package `foo` at `version` for this machine, whose launcher
/// `opt/Foo/foo` is an ELF needing `needed` and `version_needs`; `unit` adds a
/// systemd system unit.
pub fn deb_with_elf(
    dir: &Path,
    version: &str,
    needed: &[&str],
    version_needs: &[(&str, &str)],
    unit: bool,
) -> (String, PathBuf) {
    let name = format!("foo_{version}_{}.deb", host_package_arch());
    let elf = build_elf(needed, None, version_needs, &[]);
    let control = format!(
        "Package: foo\nVersion: {version}\nArchitecture: {}\nMaintainer: t <t@example.org>\n",
        host_package_arch()
    );
    let mut data = vec![E::File("opt/Foo/foo", &elf, 0o755)];
    if unit {
        data.push(E::File(
            "usr/lib/systemd/system/foo.service",
            b"[Service]\nExecStart=/opt/Foo/foo\n",
            0o644,
        ));
    }
    let path = build_deb(dir, &name, &control, &[], &data);
    (name, path)
}
