//! Reader for `newc` (`070701`) and `crc` (`070702`) cpio archives.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::path::PathBuf;

use super::stream::StreamKind;
use crate::error::{Error, Result};

const HEADER_LEN: usize = 110;
const MAX_NAME: u64 = 4096;
const MAX_LINK_TARGET: u64 = 4096;
const MAX_PENDING: usize = 65536;
const TRAILER: &[u8] = b"TRAILER!!!";

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

/// One archive record. `path` is exactly as stored.
pub(crate) struct Entry<'a> {
    pub path: PathBuf,
    pub kind: StreamKind,
    pub mode: u32,
    pub reader: &'a mut dyn Read,
}

/// Caps reads at the current record's data size.
struct Limited<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        Ok(n)
    }
}

type Key = (u32, u32, u32);

struct Pending {
    path: PathBuf,
    mode: u32,
}

pub(crate) struct Reader<R: Read> {
    src: Limited<R>,
    /// Padding bytes after the current record's data.
    pad: u64,
    /// Zero-size records with `nlink > 1` waiting for the record that holds
    /// their data.
    pending: HashMap<Key, Vec<Pending>>,
    /// Keys of `pending` in arrival order.
    order: Vec<Key>,
    pending_count: usize,
    ready: VecDeque<(PathBuf, StreamKind, u32)>,
    empty: io::Empty,
    done: bool,
}

fn truncated(e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        Error::Parse("truncated cpio archive".into())
    } else {
        Error::Parse(format!("cpio read failed: {e}"))
    }
}

fn pad4(n: u64) -> u64 {
    (4 - n % 4) % 4
}

fn field(raw: &[u8]) -> Result<u32> {
    std::str::from_utf8(raw)
        .ok()
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| Error::Parse("invalid cpio header field".into()))
}

impl<R: Read> Reader<R> {
    pub(crate) fn new(inner: R) -> Reader<R> {
        Reader {
            src: Limited { inner, left: 0 },
            pad: 0,
            pending: HashMap::new(),
            order: Vec::new(),
            pending_count: 0,
            ready: VecDeque::new(),
            empty: io::empty(),
            done: false,
        }
    }

    /// Discards what is left of the current record, then its padding.
    fn skip_rest(&mut self) -> Result<()> {
        io::copy(&mut self.src, &mut io::sink()).map_err(truncated)?;
        self.src.left = self.pad;
        self.pad = 0;
        io::copy(&mut self.src, &mut io::sink()).map_err(truncated)?;
        Ok(())
    }

    fn read_exact_vec(&mut self, n: u64) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.src.left = n;
        self.src.read_to_end(&mut buf).map_err(truncated)?;
        Ok(buf)
    }

    /// The next entry, or `None` after the trailer. Hard links come after the
    /// record that carries their data.
    pub(crate) fn next_entry(&mut self) -> Result<Option<Entry<'_>>> {
        self.skip_rest()?;
        loop {
            if let Some((path, kind, mode)) = self.ready.pop_front() {
                return Ok(Some(Entry {
                    path,
                    kind,
                    mode,
                    reader: &mut self.empty,
                }));
            }
            if self.done {
                return Ok(None);
            }

            let mut head = self.read_exact_vec(6)?;
            if head != b"070701" && head != b"070702" {
                return Err(Error::Parse("unsupported cpio format".into()));
            }
            head.extend(self.read_exact_vec(HEADER_LEN as u64 - 6)?);
            let f = |i: usize| field(&head[6 + i * 8..14 + i * 8]);
            let (ino, mode, nlink) = (f(0)?, f(1)?, f(4)?);
            let size = u64::from(f(6)?);
            let key = (f(7)?, f(8)?, ino);
            let name_size = u64::from(f(11)?);
            if name_size == 0 || name_size > MAX_NAME {
                return Err(Error::Parse("invalid cpio name length".into()));
            }

            let name = self.read_exact_vec(name_size)?;
            self.src.left = pad4(HEADER_LEN as u64 + name_size);
            io::copy(&mut self.src, &mut io::sink()).map_err(truncated)?;
            let name = match name.split_last() {
                Some((0, rest)) => rest,
                _ => return Err(Error::Parse("unterminated cpio name".into())),
            };

            if name == TRAILER {
                self.flush_pending();
                self.done = true;
                continue;
            }
            use std::os::unix::ffi::OsStrExt;
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(name));
            self.pad = pad4(size);

            let kind = match mode & S_IFMT {
                S_IFDIR => {
                    self.src.left = size;
                    StreamKind::Dir
                }
                S_IFLNK => {
                    if size > MAX_LINK_TARGET {
                        return Err(Error::Parse("cpio symlink target too long".into()));
                    }
                    let target = self.read_exact_vec(size)?;
                    StreamKind::Symlink(PathBuf::from(std::ffi::OsStr::from_bytes(&target)))
                }
                S_IFREG => {
                    if nlink > 1 {
                        if size == 0 {
                            if self.pending_count >= MAX_PENDING {
                                return Err(Error::Parse(
                                    "too many pending cpio hard links".into(),
                                ));
                            }
                            self.pending_count += 1;
                            let group = self.pending.entry(key).or_default();
                            if group.is_empty() {
                                self.order.push(key);
                            }
                            group.push(Pending { path, mode });
                            self.src.left = 0;
                            continue;
                        }
                        if let Some(group) = self.pending.remove(&key) {
                            self.pending_count -= group.len();
                            self.order.retain(|k| *k != key);
                            for p in group {
                                self.ready.push_back((
                                    p.path,
                                    StreamKind::HardLink(path.clone()),
                                    p.mode,
                                ));
                            }
                        }
                    }
                    self.src.left = size;
                    StreamKind::File
                }
                _ => {
                    self.src.left = size;
                    self.skip_rest()?;
                    continue;
                }
            };
            return Ok(Some(Entry {
                path,
                kind,
                mode,
                reader: &mut self.src,
            }));
        }
    }

    /// Groups that never received a data record are empty files: the first
    /// stays a file and the rest link to it.
    fn flush_pending(&mut self) {
        for key in std::mem::take(&mut self.order) {
            let Some(group) = self.pending.remove(&key) else {
                continue;
            };
            let mut group = group.into_iter();
            let Some(first) = group.next() else { continue };
            for p in group {
                self.ready
                    .push_back((p.path, StreamKind::HardLink(first.path.clone()), p.mode));
            }
            self.ready
                .push_front((first.path, StreamKind::File, first.mode));
        }
        self.pending_count = 0;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn record(ino: u32, mode: u32, nlink: u32, name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "070701{ino:08X}{mode:08X}{:08X}{:08X}{nlink:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}",
            0, 0, 0, data.len(), 0, 0, 0, 0, name.len() + 1, 0
        )
        .into_bytes();
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    /// A `newc` archive of `(name, mode, data or link target)` entries.
    pub(crate) fn newc(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, (name, mode, data)) in entries.iter().enumerate() {
            out.extend(record(i as u32 + 1, *mode, 1, name, data));
        }
        out.extend(record(0, 0, 1, "TRAILER!!!", b""));
        out
    }

    type Got = (PathBuf, StreamKind, u32, Vec<u8>);

    fn collect(bytes: &[u8]) -> Result<Vec<Got>> {
        let mut reader = Reader::new(bytes);
        let mut out = Vec::new();
        while let Some(e) = reader.next_entry()? {
            let mut data = Vec::new();
            e.reader.read_to_end(&mut data).map_err(truncated)?;
            out.push((e.path, e.kind, e.mode, data));
        }
        Ok(out)
    }

    #[test]
    fn cpio_reads_files_dirs_symlinks() {
        let bytes = newc(&[
            ("./usr", 0o040755, b""),
            ("./usr/x", 0o104755, b"hi"),
            ("./usr/y", 0o120777, b"x"),
        ]);
        let got = collect(&bytes).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, PathBuf::from("./usr"));
        assert_eq!(got[0].1, StreamKind::Dir);
        assert_eq!(got[1].1, StreamKind::File);
        assert_eq!(got[1].3, b"hi");
        assert_ne!(got[1].2 & 0o4000, 0);
        assert_eq!(got[2].1, StreamKind::Symlink(PathBuf::from("x")));
    }

    #[test]
    fn cpio_rejects_odc() {
        let err = collect(b"070707000000000000000000000000000000000000000000000000000000000000")
            .unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn cpio_truncated_is_parse_error() {
        let bytes = newc(&[("a", 0o100644, b"hello world")]);
        for cut in [3, 50, 120, 125, bytes.len() - 30] {
            let err = collect(&bytes[..cut]).unwrap_err();
            assert!(
                matches!(err, Error::Parse(_) | Error::Io { .. }),
                "cut {cut}"
            );
        }
    }

    #[test]
    fn cpio_hard_links_follow_their_data() {
        let mut bytes = record(7, 0o100644, 2, "a", b"");
        bytes.extend(record(7, 0o100644, 2, "b", b"data"));
        bytes.extend(record(0, 0, 1, "TRAILER!!!", b""));
        let got = collect(&bytes).unwrap();
        assert_eq!(got[0].0, PathBuf::from("b"));
        assert_eq!(got[0].1, StreamKind::File);
        assert_eq!(got[0].3, b"data");
        assert_eq!(got[1].0, PathBuf::from("a"));
        assert_eq!(got[1].1, StreamKind::HardLink(PathBuf::from("b")));
    }

    #[test]
    fn cpio_skips_device_nodes() {
        let bytes = newc(&[("dev", 0o020644, b""), ("f", 0o100644, b"z")]);
        let got = collect(&bytes).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, PathBuf::from("f"));
    }

    #[test]
    fn cpio_bounds_pending_hard_links() {
        let mut bytes = Vec::new();
        for i in 0..=MAX_PENDING as u32 {
            bytes.extend(record(i + 1, 0o100644, 2, "a", b""));
        }
        let err = collect(&bytes).unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn cpio_empty_hard_link_group_is_flushed_at_trailer() {
        let mut bytes = record(7, 0o100644, 2, "a", b"");
        bytes.extend(record(7, 0o100644, 2, "b", b""));
        bytes.extend(record(0, 0, 1, "TRAILER!!!", b""));
        let got = collect(&bytes).unwrap();
        assert_eq!(got[0].1, StreamKind::File);
        assert_eq!(got[1].1, StreamKind::HardLink(PathBuf::from("a")));
    }
}
