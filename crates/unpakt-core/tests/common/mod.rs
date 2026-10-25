#![allow(dead_code)]

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

/// A shell script that creates `marker` when run.
pub fn marker_script(marker: &Path) -> Vec<u8> {
    format!("#!/bin/sh\ntouch '{}'\n", marker.display()).into_bytes()
}

/// Writes a type 2 AppImage named `name` under `dir`: a real ELF header, the
/// `AI\x02` magic, and a gzip squashfs holding `files` right after the ELF.
/// Only `E::File` and `E::Link` entries are supported.
pub fn build_appimage(dir: &Path, name: &str, files: &[E]) -> PathBuf {
    use backhand::{FilesystemWriter, NodeHeader};

    let mut image = std::fs::read("/usr/bin/true").unwrap();
    let offset = unpakt_core::format::squashfs_offset(&image).unwrap() as usize;
    image.resize(offset, 0);
    image[8..11].copy_from_slice(b"AI\x02");

    let mut writer = FilesystemWriter::default();
    writer.set_compressor(
        backhand::FilesystemCompressor::new(backhand::compression::Compressor::Gzip, None).unwrap(),
    );
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
