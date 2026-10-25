use std::io::Read;

use super::package::{Header, inspect_package};
use super::stream::{DataSource, normalize};
use super::{Confidence, FormatPlugin, open_tar};
use crate::error::{Error, Result};
use crate::model::{Arch, Artifact, FormatKind, PackageFamily, Payload, Script};

const PKGINFO: &str = ".PKGINFO";
const INSTALL: &str = ".INSTALL";
const SNIFF_MEMBERS: usize = 8;
const MAX_META_BYTES: u64 = 1024 * 1024;
const SKIP: [&str; 5] = [PKGINFO, ".MTREE", ".BUILDINFO", INSTALL, ".CHANGELOG"];

pub struct Pacman;

impl FormatPlugin for Pacman {
    fn kind(&self) -> FormatKind {
        FormatKind::Pacman
    }

    fn sniff(&self, a: &Artifact) -> Result<Confidence> {
        let mut archive = match open_tar(&a.path) {
            Ok(archive) => archive,
            Err(Error::Parse(_)) => return Ok(Confidence::No),
            Err(e) => return Err(e),
        };
        let Ok(entries) = archive.entries() else {
            return Ok(Confidence::No);
        };
        for entry in entries.take(SNIFF_MEMBERS) {
            let Ok(entry) = entry else {
                return Ok(Confidence::No);
            };
            let Ok(raw) = entry.path() else {
                return Ok(Confidence::No);
            };
            let Ok(path) = normalize(&raw) else {
                return Ok(Confidence::No);
            };
            let mut parts = path.components();
            let Some(top) = parts.next() else {
                continue;
            };
            if parts.next().is_some() || !top.as_os_str().to_string_lossy().starts_with('.') {
                return Ok(Confidence::No);
            }
            if top.as_os_str() == PKGINFO {
                return Ok(Confidence::Yes);
            }
        }
        Ok(Confidence::No)
    }

    fn inspect(&self, a: &Artifact) -> Result<Payload> {
        let bad = |e: std::io::Error| Error::Parse(format!("invalid pacman package: {e}"));
        let mut archive = open_tar(&a.path)?;
        let mut pkginfo = None;
        let mut install = None;
        for entry in archive.entries().map_err(bad)? {
            let entry = entry.map_err(bad)?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = normalize(&entry.path().map_err(bad)?)?;
            let slot = if path == std::path::Path::new(PKGINFO) {
                &mut pkginfo
            } else if path == std::path::Path::new(INSTALL) {
                &mut install
            } else {
                continue;
            };
            if slot.is_some() {
                continue;
            }
            let mut text = Vec::new();
            entry
                .take(MAX_META_BYTES + 1)
                .read_to_end(&mut text)
                .map_err(bad)?;
            if text.len() as u64 > MAX_META_BYTES {
                return Err(Error::Refused(format!(
                    "pacman metadata file is too large: {path}",
                    path = path.display()
                )));
            }
            *slot = Some(String::from_utf8_lossy(&text).into_owned());
            if pkginfo.is_some() && install.is_some() {
                break;
            }
        }
        let pkginfo =
            pkginfo.ok_or_else(|| Error::Parse("pacman package has no .PKGINFO".into()))?;
        let info = parse_pkginfo(&pkginfo);
        let missing = |key: &str| Error::Parse(format!(".PKGINFO has no {key}"));
        let header = Header {
            family: PackageFamily::Pacman,
            name: info.pkgname.ok_or_else(|| missing("pkgname"))?,
            version: info.pkgver.ok_or_else(|| missing("pkgver"))?,
            arch: info.arch.ok_or_else(|| missing("arch"))?,
            depends: info.depends,
            scripts: install
                .map(|text| {
                    vec![Script {
                        name: INSTALL.into(),
                        text,
                    }]
                })
                .unwrap_or_default(),
            data: DataSource::Tar {
                path: a.path.clone(),
                skip: SKIP.iter().map(|s| s.to_string()).collect(),
            },
        };
        inspect_package(a, header, &Arch::current())
    }
}

#[derive(Default)]
struct PkgInfo {
    pkgname: Option<String>,
    pkgver: Option<String>,
    arch: Option<String>,
    depends: Vec<String>,
}

fn parse_pkginfo(text: &str) -> PkgInfo {
    let mut info = PkgInfo::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            continue;
        }
        match key {
            "pkgname" if info.pkgname.is_none() => info.pkgname = Some(value.into()),
            "pkgver" if info.pkgver.is_none() => info.pkgver = Some(value.into()),
            "arch" if info.arch.is_none() => info.arch = Some(value.into()),
            "depend" => info.depends.push(value.into()),
            _ => {}
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::parse_pkginfo;

    #[test]
    fn pkginfo_keys_and_repeats() {
        let i = parse_pkginfo(
            "# Generated\npkgname = foo\npkgver = 1.2.3-1\narch = x86_64\ndepend = gtk3\ndepend = nss>=3\noptdepend = x\n",
        );
        assert_eq!(i.pkgname.as_deref(), Some("foo"));
        assert_eq!(i.pkgver.as_deref(), Some("1.2.3-1"));
        assert_eq!(i.arch.as_deref(), Some("x86_64"));
        assert_eq!(i.depends, ["gtk3", "nss>=3"]);
    }
}
