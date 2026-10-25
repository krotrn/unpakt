use std::path::{Path, PathBuf};

use crate::model::{EntryCandidate, ExecKind};
use crate::naming::slug;

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
const ICON_EXTS: [&str; 3] = ["png", "svg", "xpm"];

/// What the first pass learned about one archive entry.
pub(crate) struct Record {
    /// Normalised path from the archive root, without `.` components.
    pub(crate) path: PathBuf,
    pub(crate) kind: RecordKind,
}

pub(crate) enum RecordKind {
    Dir,
    Link,
    File {
        mode: u32,
        head: Vec<u8>,
        /// Text of a `.desktop` file.
        text: Option<String>,
    },
}

pub(crate) fn is_desktop_location(rel: &Path) -> bool {
    if rel.extension().is_none_or(|e| e != "desktop") {
        return false;
    }
    rel.parent() == Some(Path::new("")) || rel.starts_with("share/applications")
}

pub(crate) fn is_shared_library(rel: &Path) -> bool {
    rel.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.ends_with(".so") || n.contains(".so.")
    })
}

pub(crate) fn candidates(items: &[(PathBuf, &Record)], name: &str) -> Vec<EntryCandidate> {
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

pub(crate) fn has_source_marker(items: &[(PathBuf, &Record)]) -> bool {
    items
        .iter()
        .any(|(rel, _)| SOURCE_MARKERS.iter().any(|m| rel == Path::new(m)))
}

pub(crate) fn icon_ext(rel: &Path) -> Option<&'static str> {
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

pub(crate) fn find_icon(
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

/// Orders candidates best first: higher score, then shorter path.
pub(crate) fn sort_candidates(candidates: &mut [EntryCandidate]) {
    candidates.sort_by(|x, y| {
        y.score
            .cmp(&x.score)
            .then_with(|| x.path.as_os_str().len().cmp(&y.path.as_os_str().len()))
    });
}
