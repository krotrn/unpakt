use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tar::EntryType;

use super::{Backend, Installed, Plan, copy_cancellable, version_dir};
use crate::cancel::Cancel;
use crate::error::{Error, IoResultExt, Result};
use crate::format::open_tar;
use crate::format::package::resolve_in_payload;
use crate::format::stream::{StreamEntry, StreamKind, for_each_entry};
use crate::model::{FormatKind, PackageMeta, Payload, PayloadIndex};
use crate::{Layout, Txn};

pub struct Extract;

impl Backend for Extract {
    fn id(&self) -> &'static str {
        "extract"
    }

    fn applicable(&self, p: &Payload) -> bool {
        matches!(
            p.format,
            FormatKind::Tarball | FormatKind::Deb | FormatKind::Rpm | FormatKind::Pacman
        )
    }

    fn install(&self, plan: &Plan, layout: &Layout, txn: &mut Txn) -> Result<Installed> {
        let entry = plan
            .entry
            .as_ref()
            .ok_or_else(|| Error::NotFound("entry point for extract".into()))?;
        let dir = version_dir(plan, layout)?;
        let app_dir = layout.apps.join(&plan.id);
        let staging = app_dir.join(staging_name(&plan.label));

        txn.create_dir_all(&app_dir)?;
        txn.create_dir_new(&staging)?;

        let cancel = txn.cancel().clone();
        match &plan.payload.package {
            Some(meta) => unpack_package(meta, &staging, &cancel)?,
            None => unpack(
                &plan.artifact.path,
                &staging,
                plan.payload.strip_prefix.as_deref(),
                &cancel,
            )?,
        }
        txn.rename(&staging, &dir)?;
        Ok(Installed {
            version_dir: dir,
            entry: entry.path.clone(),
            kind: entry.kind,
        })
    }
}

/// A staging name no earlier (interrupted) run can have used.
fn staging_name(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(".staging-{label}-{}-{nanos}", std::process::id())
}

fn unsafe_path(what: impl std::fmt::Display) -> Error {
    Error::Refused(format!("unsafe path: {what}"))
}

/// Normal components of an archive path; refuses absolute and `..` paths.
fn components(path: &Path) -> Result<Vec<&std::ffi::OsStr>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => return Err(unsafe_path(path.display())),
        }
    }
    Ok(out)
}

/// The path inside staging for an archive path, or `None` for the prefix dir itself.
fn destination(path: &Path, strip: Option<&Path>) -> Result<Option<PathBuf>> {
    let comps = components(path)?;
    let skip = match strip {
        Some(prefix) => {
            let want = components(prefix)?;
            if comps.len() < want.len() || comps[..want.len()] != want[..] {
                return Err(unsafe_path(format!(
                    "{} is outside the archive root",
                    path.display()
                )));
            }
            want.len()
        }
        None => 0,
    };
    let rest: PathBuf = comps[skip..].iter().collect();
    Ok(if rest.as_os_str().is_empty() {
        None
    } else {
        Some(rest)
    })
}

/// Creates missing parents of `rel` under `staging`, refusing any symlink or
/// non-directory among them. Nothing is ever followed.
fn prepare_parents(staging: &Path, rel: &Path) -> Result<()> {
    let mut cur = staging.to_path_buf();
    let Some(parent) = rel.parent() else {
        return Ok(());
    };
    for c in parent.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Err(unsafe_path(format!("{} is not a directory", rel.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&cur).at(&cur)?;
            }
            Err(e) => return Err(e).at(&cur),
        }
    }
    Ok(())
}

/// Whether a symlink at `rel` with `target` stays inside staging. Only
/// leading `..` components are allowed, so the result does not depend on
/// other links.
fn symlink_target_is_inside(rel: &Path, target: &Path) -> bool {
    let mut up = 0usize;
    let mut seen_normal = false;
    for c in target.components() {
        match c {
            Component::ParentDir if !seen_normal => up += 1,
            Component::Normal(_) => seen_normal = true,
            Component::CurDir => {}
            _ => return false,
        }
    }
    let depth = rel.components().count().saturating_sub(1);
    up <= depth
}

/// How symlink targets are treated while unpacking.
enum LinkPolicy<'a> {
    /// Archive links must stay inside the tree; anything else is refused.
    Strict,
    /// Absolute targets are rewritten to relative ones inside the tree when the
    /// payload index resolves them; unresolvable or escaping links are skipped.
    Rebase(&'a PayloadIndex),
}

/// The link target to create for a symlink at `rel`, or `None` to skip it.
fn link_target(rel: &Path, target: &Path, links: &LinkPolicy<'_>) -> Result<Option<PathBuf>> {
    let inside = symlink_target_is_inside(rel, target);
    let index = match links {
        LinkPolicy::Strict if inside => return Ok(Some(target.to_path_buf())),
        LinkPolicy::Strict => {
            return Err(unsafe_path(format!(
                "{} -> {}",
                rel.display(),
                target.display()
            )));
        }
        LinkPolicy::Rebase(index) => index,
    };
    if !target.has_root() {
        if inside {
            return Ok(Some(target.to_path_buf()));
        }
        tracing::warn!(link = %rel.display(), target = %target.display(), "skipping symlink that leaves the package");
        return Ok(None);
    }
    let mut dest: Vec<&std::ffi::OsStr> = Vec::new();
    for c in target.components() {
        match c {
            Component::Normal(n) => dest.push(n),
            Component::ParentDir if dest.pop().is_some() => {}
            Component::RootDir | Component::CurDir => {}
            _ => {
                tracing::warn!(link = %rel.display(), target = %target.display(), "skipping symlink that leaves the package");
                return Ok(None);
            }
        }
    }
    let dest: PathBuf = dest.into_iter().collect();
    if resolve_in_payload(index, &dest).is_none() {
        tracing::warn!(link = %rel.display(), target = %target.display(), "skipping symlink to a file the package does not contain");
        return Ok(None);
    }
    let from: Vec<_> = rel
        .parent()
        .map(|p| p.components().collect())
        .unwrap_or_default();
    let to: Vec<_> = dest.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c);
    }
    Ok(Some(out))
}

/// Writes one entry below `staging`. `entry.path` and a hard link's target are
/// already relative to the tree root.
fn write_entry(
    staging: &Path,
    entry: StreamEntry<'_>,
    links: &LinkPolicy<'_>,
    cancel: &Cancel,
) -> Result<()> {
    cancel.check()?;
    let StreamEntry {
        path: rel,
        kind,
        mode,
        reader,
    } = entry;
    let dest = staging.join(&rel);
    let mode = mode & 0o777;

    match kind {
        StreamKind::Dir => {
            prepare_parents(staging, &rel)?;
            match fs::symlink_metadata(&dest) {
                Ok(m) if m.file_type().is_dir() => {}
                Ok(_) => return Err(unsafe_path(rel.display())),
                Err(_) => fs::create_dir(&dest).at(&dest)?,
            }
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode | 0o700)).at(&dest)?;
        }
        StreamKind::File => {
            prepare_parents(staging, &rel)?;
            remove_existing_file(&dest, &rel)?;
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&dest)
                .at(&dest)?;
            copy_cancellable(reader, &mut out, cancel, &dest)?;
            drop(out);
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode)).at(&dest)?;
        }
        StreamKind::Symlink(target) => {
            let Some(target) = link_target(&rel, &target, links)? else {
                return Ok(());
            };
            prepare_parents(staging, &rel)?;
            remove_existing_file(&dest, &rel)?;
            std::os::unix::fs::symlink(&target, &dest).at(&dest)?;
        }
        StreamKind::HardLink(source_rel) => {
            prepare_parents(staging, &rel)?;
            let source = staging.join(&source_rel);
            check_regular_inside(staging, &source_rel)?;
            remove_existing_file(&dest, &rel)?;
            fs::hard_link(&source, &dest).at(&dest)?;
        }
    }
    Ok(())
}

fn unpack(
    archive_path: &Path,
    staging: &Path,
    strip: Option<&Path>,
    cancel: &Cancel,
) -> Result<()> {
    let mut archive = open_tar(archive_path)?;
    let entries = archive.entries().at(archive_path)?;
    for entry in entries {
        cancel.check()?;
        let mut entry = entry.at(archive_path)?;
        let tar_kind = entry.header().entry_type();
        if !matches!(
            tar_kind,
            EntryType::Directory
                | EntryType::Regular
                | EntryType::Continuous
                | EntryType::Symlink
                | EntryType::Link
        ) {
            if !matches!(
                tar_kind,
                EntryType::XGlobalHeader
                    | EntryType::XHeader
                    | EntryType::GNULongName
                    | EntryType::GNULongLink
            ) {
                tracing::warn!(kind = ?tar_kind, "skipping special archive entry");
            }
            continue;
        }
        let raw = entry.path().at(archive_path)?.into_owned();
        let Some(rel) = destination(&raw, strip)? else {
            continue;
        };
        let mode = entry.header().mode().at(archive_path)?;
        let kind = match tar_kind {
            EntryType::Directory => StreamKind::Dir,
            EntryType::Symlink | EntryType::Link => {
                let target = entry
                    .link_name()
                    .at(archive_path)?
                    .ok_or_else(|| unsafe_path(format!("{} has no link target", rel.display())))?
                    .into_owned();
                if tar_kind == EntryType::Symlink {
                    StreamKind::Symlink(target)
                } else {
                    let source = destination(&target, strip)?
                        .ok_or_else(|| unsafe_path(target.display()))?;
                    StreamKind::HardLink(source)
                }
            }
            _ => StreamKind::File,
        };
        write_entry(
            staging,
            StreamEntry {
                path: rel,
                kind,
                mode,
                reader: &mut entry,
            },
            &LinkPolicy::Strict,
            cancel,
        )?;
    }
    Ok(())
}

fn unpack_package(meta: &PackageMeta, staging: &Path, cancel: &Cancel) -> Result<()> {
    let links = LinkPolicy::Rebase(&meta.index);
    for_each_entry(&meta.data, |entry| {
        write_entry(staging, entry, &links, cancel)
    })
}

/// A hardlink source must be an already-extracted regular file whose
/// ancestors are all real directories.
fn check_regular_inside(staging: &Path, rel: &Path) -> Result<()> {
    let mut cur = staging.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for (i, c) in comps.iter().enumerate() {
        cur.push(c);
        let meta = fs::symlink_metadata(&cur)
            .map_err(|_| unsafe_path(format!("hard link source {}", rel.display())))?;
        let ft = meta.file_type();
        let ok = if i + 1 == comps.len() {
            ft.is_file()
        } else {
            ft.is_dir()
        };
        if !ok {
            return Err(unsafe_path(format!("hard link source {}", rel.display())));
        }
    }
    Ok(())
}

/// Clears a previous non-directory entry at `dest` so a later duplicate
/// archive entry replaces it; a directory or anything unexpected is refused.
fn remove_existing_file(dest: &Path, rel: &Path) -> Result<()> {
    match fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_dir() => Err(unsafe_path(rel.display())),
        Ok(_) => fs::remove_file(dest).at(dest),
        Err(_) => Ok(()),
    }
}
