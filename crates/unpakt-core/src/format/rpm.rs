use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::package::{Header, inspect_package};
use super::stream::DataSource;
use super::{Confidence, FormatPlugin, read_head};
use crate::error::{Error, IoResultExt, Result};
use crate::model::{Arch, Artifact, FormatKind, PackageFamily, Payload, Script};

const LEAD_MAGIC: [u8; 4] = [0xED, 0xAB, 0xEE, 0xDB];
const HEADER_MAGIC: [u8; 4] = [0x8E, 0xAD, 0xE8, 0x01];
const LEAD_LEN: u64 = 96;
const MAX_INDEX_ENTRIES: u32 = 65_536;
const MAX_STORE_BYTES: u32 = 64 * 1024 * 1024;

const TAG_NAME: u32 = 1000;
const TAG_VERSION: u32 = 1001;
const TAG_RELEASE: u32 = 1002;
const TAG_ARCH: u32 = 1022;
const TAG_SOURCERPM: u32 = 1044;
const TAG_REQUIRENAME: u32 = 1049;
const SCRIPT_TAGS: [(u32, &str); 4] = [
    (1023, "%pre"),
    (1024, "%post"),
    (1025, "%preun"),
    (1026, "%postun"),
];

const TYPE_STRING: u32 = 6;
const TYPE_STRING_ARRAY: u32 = 8;
const TYPE_I18N_STRING: u32 = 9;

pub struct Rpm;

impl FormatPlugin for Rpm {
    fn kind(&self) -> FormatKind {
        FormatKind::Rpm
    }

    fn sniff(&self, a: &Artifact) -> Result<Confidence> {
        Ok(if read_head(&a.path, 4)? == LEAD_MAGIC {
            Confidence::Yes
        } else {
            Confidence::No
        })
    }

    fn inspect(&self, a: &Artifact) -> Result<Payload> {
        let mut file = File::open(&a.path).at(&a.path)?;
        let mut lead = [0u8; LEAD_LEN as usize];
        read_exact(&mut file, &mut lead, &a.path)?;
        if lead[..4] != LEAD_MAGIC {
            return Err(Error::Parse("not an rpm file".into()));
        }
        if u16::from_be_bytes([lead[6], lead[7]]) != 0 {
            return Err(source_rpm());
        }
        let signature = read_header(&mut file, &a.path)?;
        let skip = (8 - signature.total % 8) % 8;
        let mut padding = vec![0u8; skip as usize];
        read_exact(&mut file, &mut padding, &a.path)?;
        let main = read_header(&mut file, &a.path)?;
        let offset = LEAD_LEN + signature.total + skip + main.total;

        let one = |tag: u32, what: &str| {
            main.strings(tag, 1)?
                .into_iter()
                .next()
                .ok_or_else(|| Error::Parse(format!("rpm has no {what} tag")))
        };
        if main.find(TAG_SOURCERPM).is_none() {
            return Err(source_rpm());
        }
        let name = one(TAG_NAME, "name")?;
        let version = format!(
            "{}-{}",
            one(TAG_VERSION, "version")?,
            one(TAG_RELEASE, "release")?
        );
        let arch = one(TAG_ARCH, "arch")?;
        let depends = main
            .strings(TAG_REQUIRENAME, usize::MAX)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !r.starts_with("rpmlib(") && !r.starts_with('/'))
            .collect();
        let mut scripts = Vec::new();
        for (tag, label) in SCRIPT_TAGS {
            if let Some(text) = main.strings(tag, 1)?.into_iter().next() {
                scripts.push(Script {
                    name: label.to_string(),
                    text,
                });
            }
        }
        let header = Header {
            family: PackageFamily::Rpm,
            name,
            version,
            arch,
            depends,
            scripts,
            data: DataSource::Cpio {
                path: a.path.clone(),
                offset,
            },
        };
        inspect_package(a, header, &Arch::current())
    }
}

fn source_rpm() -> Error {
    Error::Unsupported("source rpm".into())
}

fn read_exact(file: &mut File, buf: &mut [u8], path: &Path) -> Result<()> {
    file.read_exact(buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::Parse("rpm is truncated".into())
        } else {
            Error::Io {
                path: path.to_path_buf(),
                source: e,
            }
        }
    })
}

struct Entry {
    tag: u32,
    kind: u32,
    offset: u32,
    count: u32,
}

struct RpmHeader {
    entries: Vec<Entry>,
    store: Vec<u8>,
    /// Bytes the header occupies in the file, excluding any padding.
    total: u64,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn read_header(file: &mut File, path: &Path) -> Result<RpmHeader> {
    let mut head = [0u8; 16];
    read_exact(file, &mut head, path)?;
    if head[..4] != HEADER_MAGIC {
        return Err(Error::Parse("invalid rpm header magic".into()));
    }
    let count = be32(&head[8..12]);
    let size = be32(&head[12..16]);
    if count > MAX_INDEX_ENTRIES {
        return Err(Error::Parse("rpm header has too many entries".into()));
    }
    if size > MAX_STORE_BYTES {
        return Err(Error::Parse("rpm header store is too large".into()));
    }
    let mut index = vec![0u8; count as usize * 16];
    read_exact(file, &mut index, path)?;
    let mut store = Vec::new();
    file.take(u64::from(size))
        .read_to_end(&mut store)
        .at(path)?;
    if store.len() != size as usize {
        return Err(Error::Parse("rpm is truncated".into()));
    }
    let entries = index
        .as_chunks::<16>()
        .0
        .iter()
        .map(|c| Entry {
            tag: be32(&c[0..4]),
            kind: be32(&c[4..8]),
            offset: be32(&c[8..12]),
            count: be32(&c[12..16]),
        })
        .collect();
    Ok(RpmHeader {
        entries,
        store,
        total: 16 + u64::from(count) * 16 + u64::from(size),
    })
}

impl RpmHeader {
    fn find(&self, tag: u32) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// Up to `limit` strings of a string, string-array or i18n-string tag;
    /// empty when the tag is absent.
    fn strings(&self, tag: u32, limit: usize) -> Result<Vec<String>> {
        let Some(e) = self.find(tag) else {
            return Ok(Vec::new());
        };
        let wanted = match e.kind {
            TYPE_STRING_ARRAY => e.count as usize,
            TYPE_STRING | TYPE_I18N_STRING => 1,
            _ => return Err(Error::Parse(format!("rpm tag {tag} is not a string"))),
        };
        let bad = || Error::Parse(format!("rpm tag {tag} runs past its store"));
        let mut rest = self.store.get(e.offset as usize..).ok_or_else(bad)?;
        let mut out = Vec::new();
        for _ in 0..wanted.min(limit) {
            let end = rest.iter().position(|b| *b == 0).ok_or_else(bad)?;
            out.push(String::from_utf8_lossy(&rest[..end]).into_owned());
            rest = &rest[end + 1..];
        }
        Ok(out)
    }
}
