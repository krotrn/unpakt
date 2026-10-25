//! One-pass inspection shared by deb, rpm and pacman payloads.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use super::stream::{DataSource, StreamKind, for_each_entry};
use super::tree::{Record, RecordKind, candidates, find_icon, icon_ext, sort_candidates};
use crate::cancel::Cancel;
use crate::compat::elf::{ElfFacts, MAX_ELF_BYTES};
use crate::desktop::{DesktopEntry, exec_argv0};
use crate::error::{Error, Result};
use crate::model::{
    Arch, Artifact, EntryCandidate, ExecKind, FormatKind, Icon, NEUTRAL_ARCHES, PackageFamily,
    PackageMeta, Payload, PayloadIndex, Script,
};
use crate::naming::{parse_file_name, slug};

const MAX_DESKTOP_BYTES: u64 = 256 * 1024;
const MAX_ICON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SCRIPT_BYTES: u64 = 256 * 1024;
const MAX_KEPT_SCRIPT_BYTES: usize = 16 * 1024 * 1024;
const MAX_KEPT_ICON_BYTES: usize = 64 * 1024 * 1024;
const MAX_KEPT_LOOSE_DESKTOP_BYTES: usize = 1024 * 1024;
const HEAD_BYTES: u64 = 512;
const MAX_LINK_HOPS: usize = 40;

/// What a format plugin read from a package's own metadata.
pub(crate) struct Header {
    pub family: PackageFamily,
    pub name: String,
    pub version: String,
    pub arch: String,
    pub depends: Vec<String>,
    pub scripts: Vec<Script>,
    pub data: DataSource,
}

#[derive(Default)]
struct Scan {
    records: Vec<Record>,
    index: PayloadIndex,
    elf: BTreeMap<PathBuf, ElfFacts>,
    desktops: BTreeMap<PathBuf, String>,
    icons: BTreeMap<PathBuf, Vec<u8>>,
    scripts: BTreeMap<PathBuf, String>,
    /// Desktop files and package-named icons outside the standard folders,
    /// kept in case a link in a standard folder points at them.
    loose_desktops: BTreeMap<PathBuf, String>,
    loose_icons: BTreeMap<PathBuf, Vec<u8>>,
    package_name: String,
    warnings: Vec<String>,
    kept_scripts: usize,
    kept_icons: usize,
    kept_loose_desktops: usize,
}

fn host_name(arch: &Arch) -> &str {
    match arch {
        Arch::X86_64 => "x86_64",
        Arch::Aarch64 => "aarch64",
        Arch::Other(name) => name,
    }
}

pub(crate) fn inspect_package(
    a: &Artifact,
    h: Header,
    host_arch: &Arch,
    cancel: &Cancel,
) -> Result<Payload> {
    let arch = h.arch.as_str();
    if !NEUTRAL_ARCHES.contains(&arch) && !host_arch.package_aliases().contains(&arch) {
        return Err(Error::Refused(format!(
            "package is for {arch}, this machine is {}",
            host_name(host_arch)
        )));
    }
    let Scan {
        records,
        index,
        elf,
        desktops,
        icons,
        scripts,
        mut warnings,
        ..
    } = scan(&h.data, &h.name, cancel)?;

    let mut desktop_entry = None;
    let mut desktop = None;
    if let Some(text) = pick_desktop(&desktops, &h.name) {
        match DesktopEntry::parse(text) {
            Ok(entry) => {
                desktop_entry = Some(entry);
                desktop = Some(text.clone());
            }
            Err(e) => warnings.push(format!("ignoring unreadable desktop file: {e}")),
        }
    }
    let desktop_value = |key: &str| {
        desktop_entry
            .as_ref()
            .and_then(|d| d.get(key))
            .filter(|v| !v.trim().is_empty())
    };
    let name = match desktop_value("Name") {
        Some(name) => name.to_string(),
        None if h.name.is_empty() => parse_file_name(&a.file_name).0,
        None => h.name.clone(),
    };

    let items: Vec<(PathBuf, &Record)> = records.iter().map(|r| (r.path.clone(), r)).collect();
    let mut entries = candidates(&items, &name);

    if let Some(resolved) = desktop_value("Exec")
        .and_then(exec_argv0)
        .and_then(|argv0| resolve_exec(&index, &argv0))
    {
        let head = records
            .iter()
            .find(|r| r.path == resolved)
            .and_then(|r| match &r.kind {
                RecordKind::File { head, .. } => Some(head),
                _ => None,
            });
        let kind = match head {
            Some(h) if h.starts_with(b"\x7fELF") => Some(ExecKind::Elf),
            Some(h) if h.starts_with(b"#!") => Some(ExecKind::Script),
            _ => None,
        };
        if let Some(kind) = kind {
            entries.retain(|e| e.path != resolved);
            entries.push(EntryCandidate {
                path: resolved,
                kind,
                score: i32::MAX,
            });
        }
    }
    if entries.is_empty() {
        return Err(Error::Refused("no executable found".into()));
    }
    sort_candidates(&mut entries);
    let entry_script = match entries[0].kind {
        ExecKind::Script => scripts.get(&entries[0].path).cloned(),
        ExecKind::Elf => None,
    };

    let icon = pick_icon(&icons, desktop_value("Icon"), &slug(&name));

    Ok(Payload {
        format: match h.family {
            PackageFamily::Deb => FormatKind::Deb,
            PackageFamily::Rpm => FormatKind::Rpm,
            PackageFamily::Pacman => FormatKind::Pacman,
        },
        name,
        version: Some(h.version.clone()),
        strip_prefix: None,
        entries,
        desktop,
        icon,
        warnings,
        package: Some(PackageMeta {
            family: h.family,
            name: h.name,
            version: h.version,
            arch: h.arch,
            depends: h.depends,
            scripts: h.scripts,
            index,
            elf,
            entry_script,
            data: h.data,
        }),
    })
}

/// Finds the file a desktop `Exec` program refers to.
fn resolve_exec(index: &PayloadIndex, argv0: &str) -> Option<PathBuf> {
    if let Some(abs) = argv0.strip_prefix('/') {
        return resolve_in_payload(index, Path::new(abs));
    }
    ["usr/bin", "bin"]
        .iter()
        .find_map(|dir| resolve_in_payload(index, &Path::new(dir).join(argv0)))
}

/// Follows payload symlinks through every component of `path` and returns the
/// regular file it ends at. Absolute link targets start again at the payload
/// root. Gives up after 40 links and on `..` above the root.
pub(crate) fn resolve_in_payload(index: &PayloadIndex, path: &Path) -> Option<PathBuf> {
    resolve_links(index, path).filter(|p| index.files.contains(p))
}

/// Like [`resolve_in_payload`], but returns the directory `path` ends at.
pub(crate) fn resolve_dir_in_payload(index: &PayloadIndex, path: &Path) -> Option<PathBuf> {
    resolve_links(index, path).filter(|p| index.dirs.contains(p))
}

/// The path left after following every payload link in `path`.
fn resolve_links(index: &PayloadIndex, path: &Path) -> Option<PathBuf> {
    enum Step {
        Root,
        Parent,
        Name(OsString),
    }
    fn steps(path: &Path) -> impl DoubleEndedIterator<Item = Step> {
        path.components().filter_map(|c| match c {
            Component::CurDir => None,
            Component::RootDir | Component::Prefix(_) => Some(Step::Root),
            Component::ParentDir => Some(Step::Parent),
            Component::Normal(n) => Some(Step::Name(n.to_owned())),
        })
    }

    let mut stack: Vec<OsString> = Vec::new();
    let mut queue: VecDeque<Step> = steps(path).collect();
    let mut hops = 0;
    while let Some(step) = queue.pop_front() {
        match step {
            Step::Root => stack.clear(),
            Step::Parent => {
                stack.pop()?;
            }
            Step::Name(name) => {
                stack.push(name);
                let here: PathBuf = stack.iter().collect();
                if let Some(target) = index.links.get(&here) {
                    hops += 1;
                    if hops > MAX_LINK_HOPS {
                        return None;
                    }
                    stack.pop();
                    for step in steps(target).rev() {
                        queue.push_front(step);
                    }
                }
            }
        }
    }
    Some(stack.iter().collect())
}

fn pick_icon(
    icons: &BTreeMap<PathBuf, Vec<u8>>,
    desktop_icon: Option<&str>,
    app_slug: &str,
) -> Option<Icon> {
    // `find_icon` ranks paths below `usr`, the way a tarball lays them out.
    let records: Vec<Record> = icons
        .keys()
        .filter_map(|p| {
            Some(Record {
                path: p.strip_prefix("usr").ok()?.to_path_buf(),
                kind: RecordKind::File {
                    mode: 0o644,
                    head: Vec::new(),
                    text: None,
                },
            })
        })
        .collect();
    let items: Vec<(PathBuf, &Record)> = records.iter().map(|r| (r.path.clone(), r)).collect();
    let wanted = desktop_icon.map(|v| match v.trim().strip_prefix("/usr/") {
        Some(rest) => format!("/{rest}"),
        None => v.to_string(),
    });
    let rel = find_icon(&items, wanted.as_deref(), app_slug)?;
    let full = Path::new("usr").join(rel);
    let ext = icon_ext(&full)?.to_string();
    let data = icons.get(&full)?.clone();
    Some(Icon { ext, data })
}

/// Prefers `<package name>.desktop`, then the first file that is shown in
/// menus, then the first file.
fn pick_desktop<'a>(desktops: &'a BTreeMap<PathBuf, String>, name: &str) -> Option<&'a String> {
    if !name.is_empty()
        && let Some((_, text)) = desktops
            .iter()
            .find(|(p, _)| p.file_stem().is_some_and(|s| s == name))
    {
        return Some(text);
    }
    let shown = desktops.values().find(|text| {
        DesktopEntry::parse(text)
            .ok()
            .is_none_or(|d| d.get("NoDisplay").map(str::trim) != Some("true"))
    });
    shown.or_else(|| desktops.values().next())
}

fn is_desktop_file(path: &Path) -> bool {
    path.parent() == Some(Path::new("usr/share/applications"))
        && path.extension().is_some_and(|e| e == "desktop")
}

fn is_icon_file(path: &Path) -> bool {
    if icon_ext(path).is_none() {
        return false;
    }
    if path.parent() == Some(Path::new("usr/share/pixmaps")) {
        return true;
    }
    // usr/share/icons/hicolor/<size>/apps/<file>
    let parts: Vec<_> = path.components().collect();
    parts.len() == 7
        && path.starts_with("usr/share/icons/hicolor")
        && parts[5].as_os_str() == "apps"
}

fn bad_payload(e: std::io::Error) -> Error {
    Error::Parse(format!("invalid package payload: {e}"))
}

/// Appends to `buf` until it holds `limit` bytes; false when the file has
/// more than that.
fn read_more(reader: &mut dyn Read, limit: u64, buf: &mut Vec<u8>) -> Result<bool> {
    let room = limit.saturating_sub(buf.len() as u64);
    reader
        .take(room + 1)
        .read_to_end(buf)
        .map_err(bad_payload)?;
    Ok(buf.len() as u64 <= limit)
}

fn scan(data: &DataSource, package_name: &str, cancel: &Cancel) -> Result<Scan> {
    let mut s = Scan {
        package_name: package_name.to_string(),
        ..Scan::default()
    };
    let mut seen: HashMap<PathBuf, usize> = HashMap::new();
    for_each_entry(data, |entry| {
        cancel.check()?;
        let path = entry.path;
        let mut parent = path.parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            s.index.dirs.insert(p.to_path_buf());
            parent = p.parent();
        }
        let mode = entry.mode;
        let kind = match entry.kind {
            StreamKind::Dir => {
                s.index.dirs.insert(path.clone());
                RecordKind::Dir
            }
            StreamKind::Symlink(target) => {
                s.index.links.insert(path.clone(), target);
                RecordKind::Link
            }
            StreamKind::HardLink(target) => {
                s.index.files.insert(path.clone());
                if mode & 0o6000 != 0 {
                    s.index.setuid.push(path.clone());
                }
                match seen.get(&target).map(|i| &s.records[*i].kind) {
                    Some(RecordKind::File { head, .. }) => {
                        let head = head.clone();
                        if let Some(facts) = s.elf.get(&target).cloned() {
                            s.elf.insert(path.clone(), facts);
                        }
                        if let Some(text) = s.scripts.get(&target).cloned()
                            && s.kept_scripts + text.len() <= MAX_KEPT_SCRIPT_BYTES
                        {
                            s.kept_scripts += text.len();
                            s.scripts.insert(path.clone(), text);
                        }
                        RecordKind::File {
                            mode,
                            head,
                            text: None,
                        }
                    }
                    _ => RecordKind::Link,
                }
            }
            StreamKind::File => {
                s.index.files.insert(path.clone());
                if mode & 0o6000 != 0 {
                    s.index.setuid.push(path.clone());
                }
                let head = read_file(&mut s, &path, entry.reader)?;
                RecordKind::File {
                    mode,
                    head,
                    text: None,
                }
            }
        };
        seen.insert(path.clone(), s.records.len());
        s.records.push(Record { path, kind });
        Ok(())
    })?;
    adopt_linked(&mut s);
    Ok(s)
}

/// Fills in desktop files and icons that the standard folders only hold as
/// links, such as `usr/share/applications/x.desktop -> ../x/x.desktop`.
fn adopt_linked(s: &mut Scan) {
    for link in s.index.links.keys() {
        if is_desktop_file(link) && !s.desktops.contains_key(link) {
            let text = resolve_in_payload(&s.index, link)
                .and_then(|t| s.desktops.get(&t).or_else(|| s.loose_desktops.get(&t)))
                .cloned();
            if let Some(text) = text {
                s.desktops.insert(link.clone(), text);
            }
        } else if is_icon_file(link) && !s.icons.contains_key(link) {
            let data = resolve_in_payload(&s.index, link)
                .and_then(|t| s.icons.get(&t).or_else(|| s.loose_icons.get(&t)))
                .cloned();
            if let Some(data) = data {
                s.icons.insert(link.clone(), data);
            }
        }
    }
}

fn is_loose_desktop(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "desktop")
}

fn is_loose_icon(path: &Path, package_name: &str) -> bool {
    !package_name.is_empty()
        && icon_ext(path).is_some()
        && path.file_stem().is_some_and(|s| s == package_name)
}

/// Reads what the inspector needs from one regular file and returns its first
/// bytes. Bodies of other files are left unread.
fn read_file(s: &mut Scan, path: &Path, reader: &mut dyn Read) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    read_more(reader, HEAD_BYTES, &mut buf)?;
    let whole = buf.len() as u64 <= HEAD_BYTES;
    let mut head = buf.clone();
    head.truncate(HEAD_BYTES as usize);

    if buf.starts_with(b"\x7fELF") {
        if whole || read_more(reader, MAX_ELF_BYTES, &mut buf)? {
            if let Some(facts) = ElfFacts::parse(&buf) {
                s.elf.insert(path.to_path_buf(), facts);
            }
        } else {
            s.warnings
                .push(format!("{} is too large to check", path.display()));
        }
    } else if is_desktop_file(path) {
        if read_more(reader, MAX_DESKTOP_BYTES, &mut buf)? {
            s.desktops.insert(
                path.to_path_buf(),
                String::from_utf8_lossy(&buf).into_owned(),
            );
        }
    } else if is_icon_file(path) {
        if !read_more(reader, MAX_ICON_BYTES, &mut buf)? {
            s.warnings
                .push(format!("ignoring icon {}: too large", path.display()));
        } else if s.kept_icons + buf.len() <= MAX_KEPT_ICON_BYTES {
            s.kept_icons += buf.len();
            s.icons.insert(path.to_path_buf(), buf);
        }
    } else if is_loose_desktop(path) {
        if read_more(reader, MAX_DESKTOP_BYTES, &mut buf)?
            && s.kept_loose_desktops + buf.len() <= MAX_KEPT_LOOSE_DESKTOP_BYTES
        {
            s.kept_loose_desktops += buf.len();
            s.loose_desktops.insert(
                path.to_path_buf(),
                String::from_utf8_lossy(&buf).into_owned(),
            );
        }
    } else if is_loose_icon(path, &s.package_name) {
        if read_more(reader, MAX_ICON_BYTES, &mut buf)?
            && s.kept_icons + buf.len() <= MAX_KEPT_ICON_BYTES
        {
            s.kept_icons += buf.len();
            s.loose_icons.insert(path.to_path_buf(), buf);
        }
    } else if buf.starts_with(b"#!")
        && read_more(reader, MAX_SCRIPT_BYTES, &mut buf)?
        && s.kept_scripts + buf.len() <= MAX_KEPT_SCRIPT_BYTES
    {
        s.kept_scripts += buf.len();
        s.scripts.insert(
            path.to_path_buf(),
            String::from_utf8_lossy(&buf).into_owned(),
        );
    }
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[&str], links: &[(&str, &str)]) -> PayloadIndex {
        PayloadIndex {
            files: files.iter().map(PathBuf::from).collect(),
            links: links
                .iter()
                .map(|(l, t)| (PathBuf::from(l), PathBuf::from(t)))
                .collect(),
            ..PayloadIndex::default()
        }
    }

    #[test]
    fn resolves_absolute_and_relative_links() {
        let i = index(
            &["opt/Foo/foo"],
            &[
                ("usr/bin/foo", "/opt/Foo/foo"),
                ("usr/bin/bar", "foo"),
                ("opt/lnk", "Foo"),
            ],
        );
        let want = Some(PathBuf::from("opt/Foo/foo"));
        assert_eq!(resolve_in_payload(&i, Path::new("usr/bin/foo")), want);
        assert_eq!(resolve_in_payload(&i, Path::new("usr/bin/bar")), want);
        assert_eq!(resolve_in_payload(&i, Path::new("opt/lnk/foo")), want);
        assert_eq!(resolve_in_payload(&i, Path::new("opt/Foo")), None);
    }

    #[test]
    fn resolves_directories_through_links() {
        let mut i = index(&["opt/foo/bin/tool"], &[("usr/lib/foo", "/opt/foo")]);
        i.dirs = ["opt", "opt/foo", "opt/foo/bin", "usr", "usr/lib"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(
            resolve_dir_in_payload(&i, Path::new("usr/lib/foo/bin")),
            Some(PathBuf::from("opt/foo/bin"))
        );
        assert_eq!(
            resolve_dir_in_payload(&i, Path::new("opt/foo/bin/tool")),
            None
        );
    }

    #[test]
    fn link_cycles_and_escapes_resolve_to_nothing() {
        let i = index(
            &["x"],
            &[("a", "b"), ("b", "a"), ("up", "../../x"), ("d/ok", "../x")],
        );
        assert_eq!(resolve_in_payload(&i, Path::new("a")), None);
        assert_eq!(resolve_in_payload(&i, Path::new("up")), None);
        assert_eq!(resolve_in_payload(&i, Path::new("../x")), None);
        assert_eq!(
            resolve_in_payload(&i, Path::new("d/ok")),
            Some(PathBuf::from("x"))
        );
    }
}
