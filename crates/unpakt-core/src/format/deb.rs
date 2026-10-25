use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use super::package::{Header, inspect_package};
use super::stream::{DataSource, ar_members, normalize};
use super::{Confidence, FormatPlugin, decompress, peek_head, read_head};
use crate::error::{Error, IoResultExt, Result};
use crate::model::{Arch, Artifact, FormatKind, PackageFamily, Payload, Script};

const AR_MAGIC: &[u8] = b"!<arch>\n";
const FIRST_MEMBER: &[u8] = b"debian-binary";
const MAX_CONTROL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CONTROL_FILE_BYTES: u64 = 1024 * 1024;
const SCRIPTS: [&str; 4] = ["preinst", "postinst", "prerm", "postrm"];

pub struct Deb;

impl FormatPlugin for Deb {
    fn kind(&self) -> FormatKind {
        FormatKind::Deb
    }

    fn sniff(&self, a: &Artifact) -> Result<Confidence> {
        let head = read_head(&a.path, AR_MAGIC.len() + 16)?;
        let first = head.get(AR_MAGIC.len()..).unwrap_or_default();
        Ok(
            if head.starts_with(AR_MAGIC) && first.starts_with(FIRST_MEMBER) {
                Confidence::Yes
            } else {
                Confidence::No
            },
        )
    }

    fn inspect(&self, a: &Artifact) -> Result<Payload> {
        let (control, data) = read_members(&a.path)?;
        let (fields, scripts) = read_control(&control)?;
        let field = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.trim().to_string())
        };
        let missing = |key: &str| Error::Parse(format!("deb control file has no {key} field"));
        let depends = field("Depends")
            .map(|d| {
                d.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let header = Header {
            family: PackageFamily::Deb,
            name: field("Package").ok_or_else(|| missing("Package"))?,
            version: field("Version").ok_or_else(|| missing("Version"))?,
            arch: field("Architecture").ok_or_else(|| missing("Architecture"))?,
            depends,
            scripts,
            data: DataSource::ArMember {
                path: a.path.clone(),
                member: data,
            },
        };
        inspect_package(a, header, &Arch::current())
    }
}

fn unsupported(member: &str) -> Error {
    Error::Unsupported(format!("deb with {member} payload"))
}

fn bad_ar(e: std::io::Error) -> Error {
    Error::Parse(format!("invalid ar archive: {e}"))
}

/// The raw bytes of the control member and the name of the data member.
fn read_members(path: &Path) -> Result<(Vec<u8>, String)> {
    let members = ar_members(path)?;
    let mut file = std::fs::File::open(path).at(path)?;
    let mut control = None;
    let mut data = None;
    for member in members {
        let name = member.name;
        let packed = name.ends_with(".bz2") || name.ends_with(".lzma");
        if name.starts_with("control.tar") && control.is_none() {
            if packed {
                return Err(unsupported(&name));
            }
            file.seek(SeekFrom::Start(member.start)).at(path)?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(member.size.min(MAX_CONTROL_BYTES + 1))
                .read_to_end(&mut bytes)
                .map_err(bad_ar)?;
            if bytes.len() as u64 > MAX_CONTROL_BYTES {
                return Err(Error::Refused("deb control archive is too large".into()));
            }
            control = Some(bytes);
        } else if name.starts_with("data.tar") && data.is_none() {
            if packed {
                return Err(unsupported(&name));
            }
            data = Some(name);
        }
        if control.is_some() && data.is_some() {
            break;
        }
    }
    match (control, data) {
        (Some(c), Some(d)) => Ok((c, d)),
        (None, _) => Err(Error::Parse("deb has no control archive".into())),
        (_, None) => Err(Error::Parse("deb has no data archive".into())),
    }
}

type Fields = Vec<(String, String)>;

fn read_control(bytes: &[u8]) -> Result<(Fields, Vec<Script>)> {
    let bad = |e: std::io::Error| Error::Parse(format!("invalid deb control archive: {e}"));
    let mut reader = Cursor::new(bytes.to_vec());
    let head = peek_head(&mut reader).map_err(bad)?;
    let mut archive = tar::Archive::new(decompress(reader, &head)?);
    let mut control = None;
    let mut found: Vec<Script> = Vec::new();
    for entry in archive.entries().map_err(bad)? {
        let entry = entry.map_err(bad)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = normalize(&entry.path().map_err(bad)?)?;
        let Some(name) = path.to_str().map(str::to_string) else {
            continue;
        };
        if name != "control" && !SCRIPTS.contains(&name.as_str()) {
            continue;
        }
        let mut text = Vec::new();
        entry
            .take(MAX_CONTROL_FILE_BYTES)
            .read_to_end(&mut text)
            .map_err(bad)?;
        let text = String::from_utf8_lossy(&text).into_owned();
        if name == "control" {
            control = Some(text);
        } else {
            found.push(Script { name, text });
        }
    }
    let control = control.ok_or_else(|| Error::Parse("deb has no control file".into()))?;
    found.sort_by_key(|s| SCRIPTS.iter().position(|n| *n == s.name));
    Ok((parse_fields(&control), found))
}

/// Splits a control file into `(field, value)` pairs, joining continuation
/// lines.
fn parse_fields(text: &str) -> Fields {
    let mut out: Fields = Vec::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = out.last_mut() {
                value.push('\n');
                value.push_str(line.trim());
            }
        } else if let Some((key, value)) = line.split_once(':') {
            out.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_fields, read_members};
    use crate::error::Error;

    #[test]
    fn control_fields_join_continuations() {
        let f = parse_fields("Package: foo\nDescription: one\n two\n .\nDepends: a, b\n");
        assert_eq!(f[0], ("Package".into(), "foo".into()));
        assert_eq!(f[1].1, "one\ntwo\n.");
        assert_eq!(f[2], ("Depends".into(), "a, b".into()));
    }

    fn ar_with(members: &[(&str, u64, usize)]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, claimed, actual) in members {
            let header = format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{claimed:<10}`\n",
                0, 0, 0, "100644"
            );
            out.extend_from_slice(header.as_bytes());
            out.extend(std::iter::repeat_n(b'x', *actual));
            if actual % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    fn parse_err(bytes: &[u8]) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.deb");
        std::fs::write(&path, bytes).unwrap();
        matches!(read_members(&path), Err(Error::Parse(_)))
    }

    #[test]
    fn oversized_ar_members_are_refused() {
        assert!(parse_err(&ar_with(&[("//", 9_999_999_999, 4)])));
        assert!(parse_err(&ar_with(&[("debian-binary", 9_999_999_999, 4)])));
        assert!(parse_err(&ar_with(&[(
            "//",
            2 * 1024 * 1024,
            2 * 1024 * 1024
        )])));
        assert!(parse_err(b"!<arch>\nshort"));
    }
}
