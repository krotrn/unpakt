use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use backhand::{FilesystemReader, InnerNode, Node, SquashfsFileReader};
use goblin::elf::Elf;

use super::{Confidence, FormatPlugin, read_head};
use crate::cancel::Cancel;
use crate::desktop::DesktopEntry;
use crate::error::{Error, IoResultExt, Result};
use crate::model::{Artifact, FormatKind, Icon, Payload};
use crate::naming::parse_file_name;

const MAX_SYMLINK_HOPS: usize = 8;
const MAX_DESKTOP_BYTES: u64 = 256 * 1024;
const MAX_ICON_BYTES: u64 = 8 * 1024 * 1024;
const ICON_EXTS: [&str; 3] = ["png", "svg", "xpm"];
const ELF_MAGIC: &[u8] = b"\x7fELF";
/// Offset of the AppImage type marker inside the ELF identification padding.
const TYPE_MARKER_AT: usize = 8;

pub struct AppImage;

type Files<'a> = HashMap<PathBuf, &'a Node<SquashfsFileReader>>;

/// Where the squashfs image starts: right after the ELF section header table.
pub fn squashfs_offset(elf: &[u8]) -> Result<u64> {
    let header =
        Elf::parse_header(elf).map_err(|e| Error::Parse(format!("invalid ELF header: {e}")))?;
    let table = u64::from(header.e_shentsize)
        .checked_mul(u64::from(header.e_shnum))
        .and_then(|len| len.checked_add(header.e_shoff));
    table.ok_or_else(|| Error::Parse("ELF section table overflows".into()))
}

fn image_type(head: &[u8]) -> Option<u8> {
    if head.len() >= TYPE_MARKER_AT + 3
        && head.starts_with(ELF_MAGIC)
        && &head[TYPE_MARKER_AT..TYPE_MARKER_AT + 2] == b"AI"
    {
        Some(head[TYPE_MARKER_AT + 2])
    } else {
        None
    }
}

impl FormatPlugin for AppImage {
    fn kind(&self) -> FormatKind {
        FormatKind::AppImage
    }

    fn sniff(&self, a: &Artifact) -> Result<Confidence> {
        let head = read_head(&a.path, TYPE_MARKER_AT + 3)?;
        Ok(match image_type(&head) {
            Some(1 | 2) => Confidence::Yes,
            _ => Confidence::No,
        })
    }

    fn inspect(&self, a: &Artifact, cancel: &Cancel) -> Result<Payload> {
        cancel.check()?;
        let (file_name_app, file_version) = parse_file_name(&a.file_name);
        let head = read_head(&a.path, TYPE_MARKER_AT + 3)?;
        let degraded = |reason: &str| Payload {
            format: FormatKind::AppImage,
            name: file_name_app.clone(),
            version: file_version.clone(),
            strip_prefix: None,
            entries: Vec::new(),
            desktop: None,
            icon: None,
            warnings: vec![format!("could not read AppImage contents: {reason}")],
            package: None,
        };
        if image_type(&head) == Some(1) {
            return Ok(degraded("type 1 images are not inspected"));
        }
        match read_contents(a, &file_name_app, file_version.clone()) {
            Ok(payload) => Ok(payload),
            Err(e @ Error::Io { .. }) => Err(e),
            Err(e) => Ok(degraded(&e.to_string())),
        }
    }
}

fn read_contents(
    a: &Artifact,
    file_name_app: &str,
    file_version: Option<String>,
) -> Result<Payload> {
    let elf_head = read_head(&a.path, 64)?;
    let offset = squashfs_offset(&elf_head)?;
    let file = File::open(&a.path).at(&a.path)?;
    let fs = FilesystemReader::from_reader_with_offset(BufReader::new(file), offset)
        .map_err(|e| Error::Parse(e.to_string()))?;

    let files: Files = fs.files().map(|n| (normalize(&n.fullpath), n)).collect();
    let mut warnings = Vec::new();

    let desktop_text = root_desktop_file(&files)
        .map(|node| read_text(&fs, node))
        .transpose()?;
    let mut entry = None;
    if let Some(text) = &desktop_text {
        match DesktopEntry::parse(text) {
            Ok(parsed) => entry = Some(parsed),
            Err(e) => warnings.push(format!("ignoring unreadable desktop file: {e}")),
        }
    }
    let desktop = entry.as_ref().and(desktop_text);
    let name = entry
        .as_ref()
        .and_then(|d| d.get("Name"))
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| file_name_app.to_string());
    let version = entry
        .as_ref()
        .and_then(|d| d.get("X-AppImage-Version"))
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
        .or(file_version);

    let icon = find_icon(
        &fs,
        &files,
        entry.as_ref().and_then(|d| d.get("Icon")),
        &mut warnings,
    )?;

    Ok(Payload {
        format: FormatKind::AppImage,
        name,
        version,
        strip_prefix: None,
        entries: Vec::new(),
        desktop,
        icon,
        warnings,
        package: None,
    })
}

/// Drops the root and `.` components so every path is relative to the image root.
fn normalize(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect()
}

fn root_desktop_file<'a>(files: &Files<'a>) -> Option<&'a Node<SquashfsFileReader>> {
    let mut found: Vec<_> = files
        .iter()
        .filter(|(p, n)| {
            p.components().count() == 1
                && p.extension().is_some_and(|e| e == "desktop")
                && matches!(n.inner, InnerNode::File(_))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(b.0));
    found.first().map(|(_, n)| **n)
}

fn read_text(fs: &FilesystemReader, node: &Node<SquashfsFileReader>) -> Result<String> {
    let InnerNode::File(file) = &node.inner else {
        return Err(Error::Parse("desktop entry is not a regular file".into()));
    };
    let mut buf = Vec::new();
    fs.file(file)
        .reader()
        .take(MAX_DESKTOP_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| Error::Parse(format!("reading {}: {e}", node.fullpath.display())))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn find_icon(
    fs: &FilesystemReader,
    files: &Files,
    icon_name: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Option<Icon>> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(name) = icon_name.map(str::trim).filter(|n| !n.is_empty()) {
        if ICON_EXTS.iter().any(|e| name.ends_with(&format!(".{e}"))) {
            candidates.push(name.to_string());
        }
        candidates.extend(ICON_EXTS.iter().map(|e| format!("{name}.{e}")));
    }
    candidates.push(".DirIcon".to_string());

    for candidate in candidates {
        let path = PathBuf::from(&candidate);
        if path.components().count() != 1 || !files.contains_key(&path) {
            continue;
        }
        let node = match resolve(files, path) {
            Ok(node) => node,
            Err(why) => {
                warnings.push(format!("ignoring icon {candidate}: {why}"));
                return Ok(None);
            }
        };
        let InnerNode::File(file) = &node.inner else {
            continue;
        };
        let mut data = Vec::new();
        fs.file(file)
            .reader()
            .take(MAX_ICON_BYTES)
            .read_to_end(&mut data)
            .map_err(|e| Error::Parse(format!("reading {}: {e}", node.fullpath.display())))?;
        match sniff_icon_ext(&data) {
            Some(ext) => {
                return Ok(Some(Icon {
                    ext: ext.to_string(),
                    data,
                }));
            }
            None => {
                warnings.push(format!(
                    "ignoring icon {candidate}: unrecognised image format"
                ));
                return Ok(None);
            }
        }
    }
    Ok(None)
}

/// Follows symlinks inside the image; a target outside it is an error.
fn resolve<'a>(
    files: &Files<'a>,
    start: PathBuf,
) -> std::result::Result<&'a Node<SquashfsFileReader>, String> {
    let mut path = start;
    for _ in 0..=MAX_SYMLINK_HOPS {
        let node = files
            .get(&path)
            .ok_or_else(|| format!("{} is missing from the image", path.display()))?;
        let InnerNode::Symlink(link) = &node.inner else {
            return Ok(node);
        };
        if link.link.is_absolute() {
            return Err("symlink points outside the image".into());
        }
        let mut parts: Vec<_> = path.parent().map_or_else(Vec::new, |p| {
            p.components().map(|c| c.as_os_str().to_owned()).collect()
        });
        for c in link.link.components() {
            match c {
                Component::Normal(p) => parts.push(p.to_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if parts.pop().is_none() {
                        return Err("symlink points outside the image".into());
                    }
                }
                _ => return Err("symlink points outside the image".into()),
            }
        }
        path = parts.into_iter().collect();
    }
    Err("too many symlink hops".into())
}

fn sniff_icon_ext(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("png");
    }
    if data.starts_with(b"/* XPM */") {
        return Some("xpm");
    }
    let head = &data[..data.len().min(512)];
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    (trimmed.starts_with("<svg") || trimmed.starts_with("<?xml")).then_some("svg")
}
