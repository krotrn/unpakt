use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use tar::EntryType;

use super::tree::{
    Record, RecordKind, candidates, find_icon, has_source_marker, icon_ext, is_desktop_location,
    sort_candidates,
};
use super::{Confidence, FormatPlugin, open_tar};
use crate::cancel::Cancel;
use crate::desktop::DesktopEntry;
use crate::error::{Error, Result};
use crate::model::{Artifact, ExecKind, FormatKind, Icon, Payload};
use crate::naming::{parse_file_name, slug};

const MAX_DESKTOP_BYTES: u64 = 256 * 1024;
const MAX_ICON_BYTES: u64 = 8 * 1024 * 1024;

pub struct Tarball;

impl FormatPlugin for Tarball {
    fn kind(&self) -> FormatKind {
        FormatKind::Tarball
    }

    fn sniff(&self, a: &Artifact) -> Result<Confidence> {
        let archive = open_tar(&a.path)?;
        // Compressed streams that fail to decode are simply not tarballs.
        let mut block = Vec::with_capacity(512);
        let ok = archive.into_inner().take(512).read_to_end(&mut block);
        if ok.is_err() {
            return Ok(Confidence::No);
        }
        if block.len() >= 262 && &block[257..262] == b"ustar" {
            return Ok(Confidence::Yes);
        }
        let lower = a.file_name.to_ascii_lowercase();
        let tar_name = [".tar", ".tgz", ".txz", ".tbz2"]
            .iter()
            .any(|e| lower.ends_with(e))
            || lower.contains(".tar.");
        Ok(if tar_name && !block.is_empty() {
            Confidence::Maybe
        } else {
            Confidence::No
        })
    }

    fn inspect(&self, a: &Artifact, cancel: &Cancel) -> Result<Payload> {
        let records = scan(&a.path, cancel)?;
        let strip_prefix = common_prefix(&records);
        let rel = |r: &Record| -> Option<PathBuf> {
            let p = match &strip_prefix {
                Some(prefix) => r.path.strip_prefix(prefix).ok()?,
                None => r.path.as_path(),
            };
            (!p.as_os_str().is_empty()).then(|| p.to_path_buf())
        };
        let items: Vec<(PathBuf, &Record)> = records
            .iter()
            .filter_map(|r| rel(r).map(|p| (p, r)))
            .collect();

        let (file_app, version) = parse_file_name(&a.file_name);
        let mut warnings = Vec::new();

        let desktop_text = items.iter().find_map(|(p, r)| match &r.kind {
            RecordKind::File { text: Some(t), .. } if is_desktop_location(p) => Some(t.clone()),
            _ => None,
        });
        let mut desktop_entry = None;
        if let Some(text) = &desktop_text {
            match DesktopEntry::parse(text) {
                Ok(entry) => desktop_entry = Some(entry),
                Err(e) => warnings.push(format!("ignoring unreadable desktop file: {e}")),
            }
        }
        let desktop = desktop_entry.as_ref().and(desktop_text);
        let name = desktop_entry
            .as_ref()
            .and_then(|d| d.get("Name"))
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .unwrap_or(file_app);

        let mut entries = candidates(&items, &name);
        let has_elf = entries.iter().any(|e| e.kind == ExecKind::Elf);
        if !has_elf && has_source_marker(&items) {
            return Err(Error::Refused("source tarball".into()));
        }
        if entries.is_empty() {
            return Err(Error::Refused("no executable found".into()));
        }
        sort_candidates(&mut entries);

        let icon_path = find_icon(
            &items,
            desktop_entry.as_ref().and_then(|d| d.get("Icon")),
            &slug(&name),
        );
        let icon = match icon_path {
            Some(rel_path) => {
                let full = match &strip_prefix {
                    Some(prefix) => prefix.join(rel_path),
                    None => rel_path,
                };
                read_icon(&a.path, &full, &mut warnings)?
            }
            None => None,
        };

        Ok(Payload {
            format: FormatKind::Tarball,
            name,
            version,
            strip_prefix,
            entries,
            desktop,
            icon,
            warnings,
            package: None,
        })
    }
}

fn unsafe_path(what: impl std::fmt::Display) -> Error {
    Error::Refused(format!("unsafe path: {what}"))
}

/// Drops `.` components; refuses absolute paths and `..`.
fn clean(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path(path.display()));
            }
        }
    }
    Ok(out)
}

const MAX_LINK_HOPS: usize = 40;

/// Checks every symlink against the whole archive, so entry order is
/// irrelevant: no entry may sit below a symlink, and no link target may
/// resolve (following other archive symlinks) above the root.
fn check_links(links: &BTreeMap<PathBuf, PathBuf>, paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        if path.ancestors().skip(1).any(|a| links.contains_key(a)) {
            return Err(unsafe_path(path.display()));
        }
    }
    for (link, target) in links {
        let bad = || unsafe_path(format!("{} -> {}", link.display(), target.display()));
        let mut stack: Vec<OsString> = link
            .parent()
            .map(|p| p.components().map(|c| c.as_os_str().to_owned()).collect())
            .unwrap_or_default();
        let mut queue: VecDeque<Component> = target.components().collect();
        let mut hops = 0;
        while let Some(c) = queue.pop_front() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    stack.pop().ok_or_else(bad)?;
                }
                Component::RootDir | Component::Prefix(_) => return Err(bad()),
                Component::Normal(part) => {
                    stack.push(part.to_owned());
                    let here: PathBuf = stack.iter().collect();
                    if let Some(next) = links.get(&here) {
                        hops += 1;
                        if hops > MAX_LINK_HOPS {
                            return Err(bad());
                        }
                        stack.pop();
                        for c in next.components().rev() {
                            queue.push_front(c);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn scan(path: &Path, cancel: &Cancel) -> Result<Vec<Record>> {
    let mut archive = open_tar(path)?;
    let bad_tar = |e: std::io::Error| Error::Parse(format!("invalid tar archive: {e}"));
    let mut records = Vec::new();
    let mut links = BTreeMap::new();
    for entry in archive.entries().map_err(bad_tar)? {
        cancel.check()?;
        let mut entry = entry.map_err(bad_tar)?;
        let raw = entry.path().map_err(bad_tar)?.into_owned();
        let clean_path = clean(&raw)?;
        let ty = entry.header().entry_type();
        let kind = match ty {
            EntryType::Symlink | EntryType::Link => {
                let target = entry
                    .link_name()
                    .map_err(bad_tar)?
                    .map(|t| t.into_owned())
                    .ok_or_else(|| unsafe_path(format!("{} has no target", raw.display())))?;
                if ty == EntryType::Symlink {
                    links.insert(clean_path.clone(), target);
                } else {
                    clean(&target)?;
                }
                RecordKind::Link
            }
            EntryType::Directory => RecordKind::Dir,
            EntryType::Regular | EntryType::Continuous => {
                let mode = entry.header().mode().map_err(bad_tar)?;
                let mut head = Vec::with_capacity(4);
                (&mut entry)
                    .take(4)
                    .read_to_end(&mut head)
                    .map_err(bad_tar)?;
                let is_desktop = clean_path.extension().is_some_and(|e| e == "desktop");
                let text = if is_desktop && entry.size() <= MAX_DESKTOP_BYTES {
                    let mut rest = Vec::new();
                    entry.read_to_end(&mut rest).map_err(bad_tar)?;
                    head.extend(rest);
                    Some(String::from_utf8_lossy(&head).into_owned())
                } else {
                    None
                };
                RecordKind::File { mode, head, text }
            }
            _ => continue,
        };
        if clean_path.as_os_str().is_empty() {
            continue;
        }
        records.push(Record {
            path: clean_path,
            kind,
        });
    }
    let paths: Vec<PathBuf> = records.iter().map(|r| r.path.clone()).collect();
    check_links(&links, &paths)?;
    Ok(records)
}

/// The single top-level directory every entry lives under, if there is one.
fn common_prefix(records: &[Record]) -> Option<PathBuf> {
    let mut tops = BTreeSet::new();
    for r in records {
        let mut comps = r.path.components();
        let first = comps.next()?;
        let nested = comps.next().is_some();
        if !nested && !matches!(r.kind, RecordKind::Dir) {
            return None;
        }
        tops.insert(first.as_os_str().to_owned());
    }
    let mut it = tops.into_iter();
    match (it.next(), it.next()) {
        (Some(only), None) => Some(PathBuf::from(only)),
        _ => None,
    }
}
/// Second pass: reads the contents of the chosen icon file.
fn read_icon(archive_path: &Path, full: &Path, warnings: &mut Vec<String>) -> Result<Option<Icon>> {
    let ext = icon_ext(full).map(str::to_string);
    let Some(ext) = ext else {
        return Ok(None);
    };
    let mut archive = open_tar(archive_path)?;
    let bad_tar = |e: std::io::Error| Error::Parse(format!("invalid tar archive: {e}"));
    for entry in archive.entries().map_err(bad_tar)? {
        let mut entry = entry.map_err(bad_tar)?;
        if !matches!(
            entry.header().entry_type(),
            EntryType::Regular | EntryType::Continuous
        ) {
            continue;
        }
        let path = entry.path().map_err(bad_tar)?.into_owned();
        if clean(&path)? == full {
            if entry.size() > MAX_ICON_BYTES {
                warnings.push(format!(
                    "ignoring icon {}: too large ({} bytes)",
                    full.display(),
                    entry.size()
                ));
                return Ok(None);
            }
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(bad_tar)?;
            return Ok(Some(Icon { ext, data }));
        }
    }
    Ok(None)
}
