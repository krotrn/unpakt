//! Format detection. Inspection reads untrusted bytes in-process and never
//! executes anything.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use crate::error::{Error, IoResultExt, Result};
use crate::model::{Artifact, FormatKind, Payload};

pub mod appimage;
pub(crate) mod cpio;
pub mod stream;
pub mod tarball;

pub use appimage::squashfs_offset;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    No,
    Maybe,
    Yes,
}

pub trait FormatPlugin: Sync {
    fn kind(&self) -> FormatKind;
    fn sniff(&self, a: &Artifact) -> Result<Confidence>;
    fn inspect(&self, a: &Artifact) -> Result<Payload>;
}

static APPIMAGE: appimage::AppImage = appimage::AppImage;
static TARBALL: tarball::Tarball = tarball::Tarball;

fn registry() -> [&'static dyn FormatPlugin; 2] {
    [&APPIMAGE, &TARBALL]
}

/// Picks the plugin that recognises `a`, or names the unsupported format.
pub fn detect(a: &Artifact) -> Result<&'static dyn FormatPlugin> {
    let mut best: Option<(Confidence, &'static dyn FormatPlugin)> = None;
    for plugin in registry() {
        let c = plugin.sniff(a)?;
        if c > Confidence::No && best.is_none_or(|(b, _)| c > b) {
            best = Some((c, plugin));
        }
    }
    if let Some((_, plugin)) = best {
        return Ok(plugin);
    }
    let head = read_head(&a.path, 8)?;
    Err(Error::Unsupported(unsupported_name(&head).into()))
}

fn unsupported_name(head: &[u8]) -> &'static str {
    if head.starts_with(b"!<arch>\n") {
        "deb package"
    } else if head.starts_with(&[0xED, 0xAB, 0xEE, 0xDB]) {
        "rpm package"
    } else if head.starts_with(b"PK\x03\x04") {
        "zip archive"
    } else if head.starts_with(b"\x7fELF") {
        "plain executable"
    } else {
        "unknown format"
    }
}

/// Reads up to `n` leading bytes of `path`.
pub(crate) fn read_head(path: &Path, n: usize) -> Result<Vec<u8>> {
    let file = File::open(path).at(path)?;
    let mut buf = Vec::with_capacity(n);
    file.take(n as u64).read_to_end(&mut buf).at(path)?;
    Ok(buf)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    None,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
}

impl Compression {
    /// Identified by magic bytes, never by file extension.
    pub(crate) fn from_magic(head: &[u8]) -> Compression {
        if head.starts_with(&[0x1f, 0x8b]) {
            Compression::Gzip
        } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            Compression::Xz
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Compression::Zstd
        } else if head.starts_with(b"BZh") {
            Compression::Bzip2
        } else {
            Compression::None
        }
    }
}

/// Wraps `reader` in the decompressor selected by the magic bytes in `head`.
pub(crate) fn decompress(reader: impl BufRead + 'static, head: &[u8]) -> Result<Box<dyn Read>> {
    Ok(match Compression::from_magic(head) {
        Compression::None => Box::new(reader),
        Compression::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(reader)),
        Compression::Xz => Box::new(liblzma::bufread::XzDecoder::new(reader)),
        Compression::Zstd => Box::new(
            zstd::Decoder::with_buffer(reader)
                .map_err(|e| Error::Parse(format!("invalid zstd stream: {e}")))?,
        ),
        Compression::Bzip2 => Box::new(bzip2::bufread::MultiBzDecoder::new(reader)),
    })
}

/// Opens `path` as a tar stream, decompressing according to its magic bytes.
pub(crate) fn open_tar(path: &Path) -> Result<tar::Archive<Box<dyn Read>>> {
    let file = File::open(path).at(path)?;
    let mut reader = BufReader::new(file);
    let head = peek_head(&mut reader).at(path)?;
    Ok(tar::Archive::new(decompress(reader, &head)?))
}

/// Copies the first bytes of `reader` without consuming them.
pub(crate) fn peek_head(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let buf = reader.fill_buf()?;
    Ok(buf[..buf.len().min(6)].to_vec())
}
