//! One streaming view over the payload of a package: a tar, a tar inside an
//! `ar` member (deb) or a cpio archive (rpm).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use super::{cpio, decompress, peek_head};
use crate::error::{Error, IoResultExt, Result};

/// Where a package payload is read from. The inner compression is always
/// detected by magic bytes.
#[derive(Debug, Clone)]
pub enum DataSource {
    /// A (possibly compressed) tar file. `skip` lists top-level names to drop.
    Tar { path: PathBuf, skip: Vec<String> },
    /// A (possibly compressed) tar stored as a member of an `ar` archive.
    ArMember { path: PathBuf, member: String },
    /// A (possibly compressed) cpio archive starting at `offset`.
    Cpio { path: PathBuf, offset: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Dir,
    File,
    Symlink(PathBuf),
    HardLink(PathBuf),
}

pub(crate) struct StreamEntry<'a> {
    /// Normalised, relative and never empty.
    pub path: PathBuf,
    pub kind: StreamKind,
    /// The raw mode, including setuid and setgid bits.
    pub mode: u32,
    pub reader: &'a mut dyn Read,
}

/// Drops leading `/` and `.` components and refuses `..`.
pub(crate) fn normalize(raw: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                return Err(Error::Refused(format!("unsafe path: {}", raw.display())));
            }
        }
    }
    Ok(out)
}

/// Calls `f` for every directory, file, symlink and hard link in the payload.
/// Device nodes, fifos and extended headers are skipped, and so are entries
/// whose path normalises to nothing.
pub(crate) fn for_each_entry(
    src: &DataSource,
    mut f: impl FnMut(StreamEntry<'_>) -> Result<()>,
) -> Result<()> {
    match src {
        DataSource::Tar { path, skip } => {
            let file = File::open(path).at(path)?;
            tar_entries(BufReader::new(file), skip, &mut f)
        }
        DataSource::ArMember { path, member } => {
            let (file, size) = seek_ar_member(path, member)?;
            tar_entries(BufReader::new(file.take(size)), &[], &mut f)
        }
        DataSource::Cpio { path, offset } => {
            let mut file = File::open(path).at(path)?;
            file.seek(SeekFrom::Start(*offset)).at(path)?;
            let mut reader = BufReader::new(file);
            let head = peek_head(&mut reader).at(path)?;
            let mut cpio = cpio::Reader::new(decompress(reader, &head)?);
            while let Some(entry) = cpio.next_entry()? {
                let path = normalize(&entry.path)?;
                let kind = match entry.kind {
                    StreamKind::HardLink(target) => StreamKind::HardLink(normalize(&target)?),
                    other => other,
                };
                if path.as_os_str().is_empty() {
                    continue;
                }
                f(StreamEntry {
                    path,
                    kind,
                    mode: entry.mode,
                    reader: entry.reader,
                })?;
            }
            Ok(())
        }
    }
}

fn tar_entries(
    mut reader: BufReader<impl Read + 'static>,
    skip: &[String],
    f: &mut impl FnMut(StreamEntry<'_>) -> Result<()>,
) -> Result<()> {
    let bad = |e: std::io::Error| Error::Parse(format!("invalid tar archive: {e}"));
    let head = peek_head(&mut reader).map_err(bad)?;
    let mut archive = tar::Archive::new(decompress(reader, &head)?);
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let path = normalize(&entry.path().map_err(bad)?)?;
        let Some(top) = path.components().next() else {
            continue;
        };
        if skip.iter().any(|s| top.as_os_str() == s.as_str()) {
            continue;
        }
        let kind = match entry.header().entry_type() {
            tar::EntryType::Directory => StreamKind::Dir,
            tar::EntryType::Regular | tar::EntryType::Continuous => StreamKind::File,
            tar::EntryType::Symlink => StreamKind::Symlink(link_target(&entry)?),
            tar::EntryType::Link => StreamKind::HardLink(normalize(&link_target(&entry)?)?),
            _ => continue,
        };
        let mode = entry.header().mode().map_err(bad)?;
        f(StreamEntry {
            path,
            kind,
            mode,
            reader: &mut entry,
        })?;
    }
    Ok(())
}

fn link_target<R: Read>(entry: &tar::Entry<'_, R>) -> Result<PathBuf> {
    let target = entry
        .link_name()
        .map_err(|e| Error::Parse(format!("invalid tar archive: {e}")))?
        .ok_or_else(|| Error::Parse("tar link without a target".into()))?;
    Ok(target.into_owned())
}

/// Opens `path` positioned at the data of the ar member `member`; returns the
/// file and the member's size.
fn seek_ar_member(path: &Path, member: &str) -> Result<(File, u64)> {
    let file = File::open(path).at(path)?;
    let bad = |e: std::io::Error| Error::Parse(format!("invalid ar archive: {e}"));
    let mut archive = ar::Archive::new(&file);
    let mut found = None;
    while let Some(entry) = archive.next_entry() {
        let entry = entry.map_err(bad)?;
        if entry.header().identifier() == member.as_bytes() {
            let start = (&file).stream_position().at(path)?;
            found = Some((start, entry.header().size()));
            break;
        }
    }
    drop(archive);
    let Some((start, size)) = found else {
        return Err(Error::Parse(format!("ar member `{member}` not found")));
    };
    let mut file = file;
    file.seek(SeekFrom::Start(start)).at(path)?;
    Ok((file, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::cpio::tests::newc;
    use std::io::Write;

    fn tar_bytes(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_mode(0o755);
            match data {
                Some(d) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(d.len() as u64);
                    h.set_cksum();
                    b.append_data(&mut h, name, *d).unwrap();
                }
                None => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    h.set_cksum();
                    b.append_data(&mut h, name, &b""[..]).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    fn collect(src: &DataSource) -> Result<Vec<(PathBuf, StreamKind)>> {
        let mut out = Vec::new();
        for_each_entry(src, |e| {
            out.push((e.path, e.kind));
            Ok(())
        })?;
        Ok(out)
    }

    #[test]
    fn normalize_strips_dot_and_root() {
        assert_eq!(
            normalize(Path::new("./usr/bin/x")).unwrap(),
            PathBuf::from("usr/bin/x")
        );
        assert_eq!(
            normalize(Path::new("/usr/bin/x")).unwrap(),
            PathBuf::from("usr/bin/x")
        );
        assert_eq!(normalize(Path::new("./")).unwrap(), PathBuf::new());
        assert!(matches!(
            normalize(Path::new("a/../b")),
            Err(Error::Refused(_))
        ));
    }

    #[test]
    fn stream_reads_ar_member() {
        let dir = tempfile::tempdir().unwrap();
        let tar = tar_bytes(&[("./", None), ("./opt/", None), ("./opt/a", Some(b"x"))]);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        let gz = gz.finish().unwrap();
        let path = dir.path().join("p.deb");
        let mut b = ar::Builder::new(File::create(&path).unwrap());
        b.append(
            &ar::Header::new(b"debian-binary".to_vec(), 4),
            &b"2.0\n"[..],
        )
        .unwrap();
        b.append(
            &ar::Header::new(b"data.tar.gz".to_vec(), gz.len() as u64),
            &gz[..],
        )
        .unwrap();
        drop(b);
        let src = DataSource::ArMember {
            path,
            member: "data.tar.gz".into(),
        };
        assert_eq!(
            collect(&src).unwrap(),
            vec![
                (PathBuf::from("opt"), StreamKind::Dir),
                (PathBuf::from("opt/a"), StreamKind::File),
            ]
        );
    }

    #[test]
    fn stream_missing_ar_member_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.deb");
        let mut b = ar::Builder::new(File::create(&path).unwrap());
        b.append(
            &ar::Header::new(b"debian-binary".to_vec(), 4),
            &b"2.0\n"[..],
        )
        .unwrap();
        drop(b);
        let src = DataSource::ArMember {
            path,
            member: "data.tar.xz".into(),
        };
        assert!(matches!(collect(&src), Err(Error::Parse(_))));
    }

    #[test]
    fn stream_tar_skips_listed_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.tar");
        std::fs::write(
            &path,
            tar_bytes(&[
                (".PKGINFO", Some(b"a")),
                (".MTREE", Some(b"b")),
                ("usr/x", Some(b"c")),
            ]),
        )
        .unwrap();
        let src = DataSource::Tar {
            path,
            skip: vec![".PKGINFO".into(), ".MTREE".into()],
        };
        assert_eq!(
            collect(&src).unwrap(),
            vec![(PathBuf::from("usr/x"), StreamKind::File)]
        );
    }

    #[test]
    fn stream_cpio_honours_offset_and_compression() {
        let dir = tempfile::tempdir().unwrap();
        let raw = newc(&[("./usr", 0o040755, b""), ("./usr/x", 0o100755, b"hi")]);
        let gz = zstd::stream::encode_all(&raw[..], 1).unwrap();
        let path = dir.path().join("p.rpm");
        let mut bytes = vec![0xAA; 16];
        bytes.extend(gz);
        std::fs::write(&path, bytes).unwrap();
        let src = DataSource::Cpio { path, offset: 16 };
        let mut seen = Vec::new();
        for_each_entry(&src, |e| {
            let mut data = Vec::new();
            e.reader.read_to_end(&mut data).unwrap();
            seen.push((e.path, data));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec![
                (PathBuf::from("usr"), vec![]),
                (PathBuf::from("usr/x"), b"hi".to_vec()),
            ]
        );
    }
}
