//! Makes an installed version visible: launcher entry, icon and command on PATH.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::desktop::{DesktopEntry, exec_line, field_codes};
use crate::error::{Error, Result};
use crate::manifest::{FileKind, FileRecord};
use crate::model::{ExecKind, Icon};
use crate::naming::validate_command;
use crate::paths::Layout;
use crate::txn::Txn;

const PNG_SIZES: [u32; 10] = [16, 22, 24, 32, 48, 64, 96, 128, 256, 512];
const NO_FUSE_PREFIX: &str = "env APPIMAGE_EXTRACT_AND_RUN=1 ";

pub struct IntegrateReq<'a> {
    pub id: &'a str,
    pub name: &'a str,
    /// Absolute path of the launched program, via `apps/<id>/current/…`.
    pub exec: &'a Path,
    pub kind: ExecKind,
    pub desktop_src: Option<&'a str>,
    pub icon: Option<&'a Icon>,
    pub want_desktop: bool,
    pub command: Option<&'a str>,
    pub force: bool,
    pub no_fuse_appimage: bool,
    /// Files already recorded for this app (re-integration).
    pub owned: &'a [FileRecord],
    pub path_var: &'a OsStr,
}

#[derive(Debug)]
pub struct Integration {
    pub files: Vec<FileRecord>,
    pub warnings: Vec<String>,
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The first PATH dir that would run something else when `command` is typed:
/// one holding an entry of that name and listed before `own_bin` (or any dir
/// when `own_bin` is not on PATH).
pub fn find_shadow(command: &str, path_var: &OsStr, own_bin: &Path) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if same_dir(&dir, own_bin) {
            return None;
        }
        if exists(&dir.join(command)) {
            return Some(dir);
        }
    }
    None
}

fn icon_target(layout: &Layout, id: &str, icon: &Icon) -> Option<PathBuf> {
    let name = format!("unpakt-{id}");
    match icon.ext.as_str() {
        "svg" => Some(layout.icons.join(format!("scalable/apps/{name}.svg"))),
        "png" => {
            let width = icon
                .data
                .get(16..20)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            let size = width
                .and_then(|w| PNG_SIZES.into_iter().find(|s| *s >= w))
                .unwrap_or(512);
            Some(layout.icons.join(format!("{size}x{size}/apps/{name}.png")))
        }
        _ => None,
    }
}

fn build_desktop(req: &IntegrateReq, has_icon: bool, warnings: &mut Vec<String>) -> DesktopEntry {
    let parsed = req
        .desktop_src
        .and_then(|src| match DesktopEntry::parse(src) {
            Ok(entry) => Some(entry),
            Err(e) => {
                warnings.push(format!("ignoring the bundled desktop file: {e}"));
                None
            }
        });
    let fresh = DesktopEntry::new_application(req.name);
    let mut entry = parsed.unwrap_or_else(|| fresh.clone());
    for key in ["Type", "Name"] {
        if entry.get(key).is_none()
            && let Some(v) = fresh.get(key)
        {
            entry.set(key, v);
        }
    }
    let codes = entry.get("Exec").map(field_codes).unwrap_or_default();
    let mut exec = exec_line(req.exec, &codes);
    if req.no_fuse_appimage {
        exec.insert_str(0, NO_FUSE_PREFIX);
    }
    entry.set("Exec", &exec);
    if has_icon {
        entry.set("Icon", &format!("unpakt-{}", req.id));
    } else {
        entry.remove("Icon");
    }
    entry.set("X-Unpakt-Id", req.id);
    entry.remove("DBusActivatable");
    entry.remove("TryExec");
    entry
}

fn is_owned(owned: &[FileRecord], p: &Path) -> bool {
    owned.iter().any(|f| f.path == p)
}

/// Fails on a foreign file unless forced; otherwise moves a foreign file aside
/// so that rollback restores it.
fn claim(p: &Path, req: &IntegrateReq, txn: &mut Txn) -> Result<()> {
    refuse_unless_file_or_link(p)?;
    if exists(p) && !is_owned(req.owned, p) {
        if !req.force {
            return Err(Error::Conflict(format!(
                "{} already exists and is not managed by unpakt; use --force to replace it",
                p.display()
            )));
        }
        txn.remove_file(p)?;
    }
    Ok(())
}

/// Directories and special files are never ours to replace, forced or not.
fn refuse_unless_file_or_link(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if !m.is_file() && !m.is_symlink() => Err(Error::Refused(format!(
            "{} is not a regular file or symlink; refusing to replace it",
            p.display()
        ))),
        _ => Ok(()),
    }
}

fn wrapper(exec: &Path) -> String {
    let quoted = exec.to_string_lossy().replace('\'', "'\\''");
    format!("#!/bin/sh\nexec '{quoted}' \"$@\"\n")
}

pub fn integrate(layout: &Layout, req: &IntegrateReq, txn: &mut Txn) -> Result<Integration> {
    let mut warnings = Vec::new();
    let mut files = Vec::new();

    let desktop_path = layout
        .applications
        .join(format!("unpakt-{}.desktop", req.id));
    let mut icon_path = None;
    if req.want_desktop
        && let Some(icon) = req.icon
    {
        icon_path = icon_target(layout, req.id, icon);
        if icon_path.is_none() {
            warnings.push(format!(
                "icon format {} is not supported; skipped",
                icon.ext
            ));
        }
    }
    if let Some(command) = req.command {
        validate_command(command)?;
    }
    let bin_path = req.command.map(|c| layout.bin.join(c));

    // Refuse before touching anything.
    // A command the app already owns was accepted when it was first added.
    let owns_bin = bin_path.as_deref().is_some_and(|p| is_owned(req.owned, p));
    if let (Some(command), false) = (req.command, req.force || owns_bin)
        && let Some(dir) = find_shadow(command, req.path_var, &layout.bin)
    {
        return Err(Error::Conflict(format!(
            "`{command}` would be shadowed by {}; use --force or choose another command",
            dir.join(command).display()
        )));
    }
    let mut targets = vec![bin_path.as_deref(), icon_path.as_deref()];
    if req.want_desktop {
        targets.push(Some(&desktop_path));
    }
    for p in targets.into_iter().flatten() {
        refuse_unless_file_or_link(p)?;
        if !req.force && exists(p) && !is_owned(req.owned, p) {
            return Err(Error::Conflict(format!(
                "{} already exists and is not managed by unpakt; use --force to replace it",
                p.display()
            )));
        }
    }

    if req.want_desktop {
        txn.create_dir_all(&layout.applications)?;
        claim(&desktop_path, req, txn)?;
        let entry = build_desktop(req, icon_path.is_some(), &mut warnings);
        txn.write(&desktop_path, entry.render().as_bytes(), 0o644)?;
        files.push(FileRecord {
            path: desktop_path,
            kind: FileKind::Desktop,
        });
    }

    if let (Some(path), Some(icon)) = (icon_path, req.icon) {
        if let Some(parent) = path.parent() {
            txn.create_dir_all(parent)?;
        }
        claim(&path, req, txn)?;
        txn.write(&path, &icon.data, 0o644)?;
        files.push(FileRecord {
            path,
            kind: FileKind::Icon,
        });
    }

    if let Some(path) = bin_path {
        txn.create_dir_all(&layout.bin)?;
        claim(&path, req, txn)?;
        match req.kind {
            ExecKind::Elf => {
                // An owned wrapper cannot be swapped in place.
                if exists(&path) && !fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
                    txn.remove_file(&path)?;
                }
                txn.symlink_swap(req.exec, &path)?;
            }
            ExecKind::Script => {
                if exists(&path) && fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink()) {
                    txn.remove_file(&path)?;
                }
                txn.write(&path, wrapper(req.exec).as_bytes(), 0o755)?;
            }
        }
        files.push(FileRecord {
            path,
            kind: FileKind::Bin,
        });
        let on_path = std::env::split_paths(req.path_var).any(|d| same_dir(&d, &layout.bin));
        if !on_path {
            warnings.push(format!("{} is not in PATH", layout.bin.display()));
        }
    }

    // Files an earlier version created that this one does not.
    for stale in req
        .owned
        .iter()
        .filter(|o| !files.iter().any(|f| f.path == o.path))
    {
        if exists(&stale.path) {
            txn.remove_file(&stale.path)?;
        }
        if stale.kind == FileKind::Icon {
            prune_icon_dirs(&stale.path, txn)?;
        }
    }

    Ok(Integration { files, warnings })
}

/// A dir is empty when everything left in it is a path `txn` set aside itself.
fn dir_is_empty(dir: &Path, txn: &Txn) -> bool {
    match fs::read_dir(dir) {
        Ok(entries) => entries.flatten().all(|e| txn.owns(&e.path())),
        Err(_) => false,
    }
}

/// Removes empty icon dirs below the theme dir (`hicolor`), starting at the
/// file's parent. The theme dir itself is never removed.
fn prune_icon_dirs(icon: &Path, txn: &mut Txn) -> Result<()> {
    let Some(mut dir) = icon.parent().map(Path::to_path_buf) else {
        return Ok(());
    };
    let Some(stop) = dir.parent().and_then(Path::parent).map(Path::to_path_buf) else {
        return Ok(());
    };
    while dir != stop && dir.starts_with(&stop) && dir_is_empty(&dir, txn) {
        txn.remove_dir_all(&dir)?;
        if !dir.pop() {
            break;
        }
    }
    Ok(())
}

/// Removes the recorded files; files that are already gone are skipped.
pub fn disintegrate(files: &[FileRecord], txn: &mut Txn) -> Result<()> {
    for f in files {
        if exists(&f.path) {
            txn.remove_file(&f.path)?;
        }
        if f.kind == FileKind::Icon {
            prune_icon_dirs(&f.path, txn)?;
        }
    }
    Ok(())
}
