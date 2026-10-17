use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use tar::EntryType;

use super::{Backend, Installed, Plan, version_dir};
use crate::error::{Error, IoResultExt, Result};
use crate::format::open_tar;
use crate::model::{FormatKind, Payload};
use crate::{Layout, Txn};

pub struct Extract;

impl Backend for Extract {
    fn id(&self) -> &'static str {
        "extract"
    }

    fn applicable(&self, p: &Payload) -> bool {
        p.format == FormatKind::Tarball
    }

    fn install(&self, plan: &Plan, layout: &Layout, txn: &mut Txn) -> Result<Installed> {
        let entry = plan
            .entry
            .as_ref()
            .ok_or_else(|| Error::NotFound("entry point for extract".into()))?;
        let dir = version_dir(plan, layout)?;
        let app_dir = layout.apps.join(&plan.id);
        let staging = app_dir.join(format!(".staging-{}", plan.label));

        txn.create_dir_all(&app_dir)?;
        fs::create_dir(&staging).at(&staging)?;
        txn.track_new(&staging);

        unpack(
            &plan.artifact.path,
            &staging,
            plan.payload.strip_prefix.as_deref(),
        )?;
        txn.rename(&staging, &dir)?;
        Ok(Installed {
            version_dir: dir,
            entry: entry.path.clone(),
            kind: entry.kind,
        })
    }
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

fn unpack(archive_path: &Path, staging: &Path, strip: Option<&Path>) -> Result<()> {
    let mut archive = open_tar(archive_path)?;
    let entries = archive.entries().at(archive_path)?;
    for entry in entries {
        let mut entry = entry.at(archive_path)?;
        let kind = entry.header().entry_type();
        if !matches!(
            kind,
            EntryType::Directory
                | EntryType::Regular
                | EntryType::Continuous
                | EntryType::Symlink
                | EntryType::Link
        ) {
            if !matches!(
                kind,
                EntryType::XGlobalHeader
                    | EntryType::XHeader
                    | EntryType::GNULongName
                    | EntryType::GNULongLink
            ) {
                tracing::warn!(kind = ?kind, "skipping special archive entry");
            }
            continue;
        }
        let raw = entry.path().at(archive_path)?.into_owned();
        let Some(rel) = destination(&raw, strip)? else {
            continue;
        };
        let dest = staging.join(&rel);
        let mode = entry.header().mode().at(archive_path)? & 0o777;

        match kind {
            EntryType::Directory => {
                prepare_parents(staging, &rel)?;
                match fs::symlink_metadata(&dest) {
                    Ok(m) if m.file_type().is_dir() => {}
                    Ok(_) => return Err(unsafe_path(rel.display())),
                    Err(_) => fs::create_dir(&dest).at(&dest)?,
                }
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode | 0o700)).at(&dest)?;
            }
            EntryType::Regular | EntryType::Continuous => {
                prepare_parents(staging, &rel)?;
                remove_existing_file(&dest, &rel)?;
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode)
                    .open(&dest)
                    .at(&dest)?;
                std::io::copy(&mut entry as &mut dyn Read, &mut out).at(&dest)?;
                drop(out);
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode)).at(&dest)?;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .at(archive_path)?
                    .ok_or_else(|| unsafe_path(format!("{} has no link target", rel.display())))?
                    .into_owned();
                if !symlink_target_is_inside(&rel, &target) {
                    return Err(unsafe_path(format!(
                        "{} -> {}",
                        rel.display(),
                        target.display()
                    )));
                }
                prepare_parents(staging, &rel)?;
                remove_existing_file(&dest, &rel)?;
                std::os::unix::fs::symlink(&target, &dest).at(&dest)?;
            }
            EntryType::Link => {
                let target = entry
                    .link_name()
                    .at(archive_path)?
                    .ok_or_else(|| unsafe_path(format!("{} has no link target", rel.display())))?
                    .into_owned();
                let source_rel =
                    destination(&target, strip)?.ok_or_else(|| unsafe_path(target.display()))?;
                prepare_parents(staging, &rel)?;
                let source = staging.join(&source_rel);
                check_regular_inside(staging, &source_rel)?;
                remove_existing_file(&dest, &rel)?;
                fs::hard_link(&source, &dest).at(&dest)?;
            }
            _ => unreachable!("other entry types were skipped above"),
        }
    }
    Ok(())
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
