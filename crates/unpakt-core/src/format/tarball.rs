use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use tar::EntryType;

use super::{Confidence, FormatPlugin, open_tar};
use crate::desktop::DesktopEntry;
use crate::error::{Error, Result};
use crate::model::{Artifact, EntryCandidate, ExecKind, FormatKind, Icon, Payload};
use crate::naming::{parse_file_name, slug};

const SOURCE_MARKERS: [&str; 9] = [
    "configure",
    "CMakeLists.txt",
    "meson.build",
    "Makefile.am",
    "Cargo.toml",
    "setup.py",
    "pyproject.toml",
    "package.json",
    "go.mod",
];
const PENALISED_DIRS: [&str; 5] = ["lib", "libexec", "share", "test", "tests"];
const MAX_DESKTOP_BYTES: u64 = 256 * 1024;
const MAX_ICON_BYTES: u64 = 8 * 1024 * 1024;
const ICON_EXTS: [&str; 3] = ["png", "svg", "xpm"];

pub struct Tarball;

/// What the first pass learned about one archive entry.
struct Record {
    /// Normalised path from the archive root, without `.` components.
    path: PathBuf,
    kind: RecordKind,
}

enum RecordKind {
    Dir,
    Link,
    File {
        mode: u32,
        head: Vec<u8>,
        /// Text of a `.desktop` file.
        text: Option<String>,
    },
}

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

    fn inspect(&self, a: &Artifact) -> Result<Payload> {
        let records = scan(&a.path)?;
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
        entries.sort_by(|x, y| {
            y.score
                .cmp(&x.score)
                .then_with(|| x.path.as_os_str().len().cmp(&y.path.as_os_str().len()))
        });

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

fn scan(path: &Path) -> Result<Vec<Record>> {
    let mut archive = open_tar(path)?;
    let bad_tar = |e: std::io::Error| Error::Parse(format!("invalid tar archive: {e}"));
    let mut records = Vec::new();
    let mut links = BTreeMap::new();
    for entry in archive.entries().map_err(bad_tar)? {
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

fn is_desktop_location(rel: &Path) -> bool {
    if rel.extension().is_none_or(|e| e != "desktop") {
        return false;
    }
    rel.parent() == Some(Path::new("")) || rel.starts_with("share/applications")
}

fn is_shared_library(rel: &Path) -> bool {
    rel.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.ends_with(".so") || n.contains(".so.")
    })
}

fn candidates(items: &[(PathBuf, &Record)], name: &str) -> Vec<EntryCandidate> {
    let wanted = slug(name);
    let mut out = Vec::new();
    for (rel, record) in items {
        let RecordKind::File { mode, head, .. } = &record.kind else {
            continue;
        };
        if mode & 0o111 == 0 || is_shared_library(rel) {
            continue;
        }
        let kind = if head.starts_with(b"\x7fELF") {
            ExecKind::Elf
        } else if head.starts_with(b"#!") {
            ExecKind::Script
        } else {
            continue;
        };
        out.push(EntryCandidate {
            path: rel.clone(),
            kind,
            score: score(rel, kind, &wanted),
        });
    }
    out
}

fn score(rel: &Path, kind: ExecKind, wanted: &str) -> i32 {
    let mut score = 0;
    let stem = rel
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !wanted.is_empty() {
        if stem == wanted {
            score += 50;
        } else if stem.starts_with(wanted) {
            score += 25;
        }
    }
    if rel
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|d| d == "bin")
    {
        score += 20;
    }
    if kind == ExecKind::Elf {
        score += 10;
    }
    let penalised = rel.components().any(|c| {
        PENALISED_DIRS
            .iter()
            .any(|d| c.as_os_str() == std::ffi::OsStr::new(d))
    });
    if penalised {
        score -= 30;
    }
    score
}

fn has_source_marker(items: &[(PathBuf, &Record)]) -> bool {
    items
        .iter()
        .any(|(rel, _)| SOURCE_MARKERS.iter().any(|m| rel == Path::new(m)))
}

fn icon_ext(rel: &Path) -> Option<&'static str> {
    let ext = rel.extension()?.to_string_lossy().to_ascii_lowercase();
    ICON_EXTS.iter().find(|e| **e == ext).copied()
}

/// Matches `name` (with or without an icon extension) against a file name.
fn icon_name_matches(rel: &Path, name: &str, exts: &[&str]) -> bool {
    let Some(file) = rel.file_name().map(|f| f.to_string_lossy()) else {
        return false;
    };
    if file == name {
        return icon_ext(rel).is_some_and(|e| exts.contains(&e));
    }
    match icon_ext(rel) {
        Some(e) if exts.contains(&e) => rel.file_stem().is_some_and(|s| s == name),
        _ => false,
    }
}

/// Edge length of a `hicolor/<size>/apps` directory; scalable counts as largest.
fn size_rank(dir: &str) -> Option<u32> {
    if dir == "scalable" {
        return Some(u32::MAX);
    }
    dir.split_once('x')?.0.parse().ok()
}

fn find_icon(
    items: &[(PathBuf, &Record)],
    desktop_icon: Option<&str>,
    app_slug: &str,
) -> Option<PathBuf> {
    let files = || {
        items
            .iter()
            .filter(|(_, r)| matches!(r.kind, RecordKind::File { .. }))
            .map(|(p, _)| p)
    };
    let Some(value) = desktop_icon.map(str::trim).filter(|v| !v.is_empty()) else {
        let candidates = [format!("{app_slug}.png"), format!("{app_slug}.svg")];
        return files()
            .find(|p| {
                p.parent() == Some(Path::new(""))
                    && p.to_str()
                        .is_some_and(|s| candidates.iter().any(|c| c == s))
            })
            .cloned();
    };

    if let Some(abs) = value.strip_prefix('/') {
        let wanted = Path::new(abs);
        if let Some(p) = files().find(|p| p.as_path() == wanted && icon_ext(p).is_some()) {
            return Some(p.clone());
        }
    }
    let name = Path::new(value)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| value.to_string());

    let hicolor = files()
        .filter(|p| {
            p.starts_with("share/icons/hicolor") && icon_name_matches(p, &name, &["png", "svg"])
        })
        .filter_map(|p| {
            let comps: Vec<_> = p.components().collect();
            // share/icons/hicolor/<size>/apps/<file>
            let size = comps.get(3)?.as_os_str().to_str().and_then(size_rank)?;
            (comps.get(4)?.as_os_str() == "apps").then_some((size, p))
        })
        .max_by_key(|(size, _)| *size)
        .map(|(_, p)| p.clone());
    if hicolor.is_some() {
        return hicolor;
    }
    let pixmaps = files()
        .find(|p| {
            p.parent() == Some(Path::new("share/pixmaps"))
                && icon_name_matches(p, &name, &ICON_EXTS)
        })
        .cloned();
    if pixmaps.is_some() {
        return pixmaps;
    }
    files()
        .find(|p| p.parent() == Some(Path::new("")) && icon_name_matches(p, &name, &ICON_EXTS))
        .cloned()
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
