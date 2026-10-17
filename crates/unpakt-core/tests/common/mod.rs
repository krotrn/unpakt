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
    const DB: [&str; 4] = [
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
