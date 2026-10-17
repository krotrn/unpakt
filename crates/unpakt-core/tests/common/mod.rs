#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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
