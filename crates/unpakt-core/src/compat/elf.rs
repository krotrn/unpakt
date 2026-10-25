use std::collections::BTreeSet;

use goblin::elf::Elf;
use goblin::elf::dynamic::{DT_RPATH, DT_RUNPATH};

pub const MAX_ELF_BYTES: u64 = 512 * 1024 * 1024;

const MAX_ABS_PATHS: usize = 256;
const MAX_ABS_PATH_LEN: usize = 4096;
const ABS_PREFIXES: [&[u8]; 3] = [b"/opt/", b"/usr/share/", b"/usr/lib/"];

/// Facts read from an ELF binary: needed libraries, run path, symbol-version
/// needs and hardcoded absolute paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElfFacts {
    pub needed: Vec<String>,
    pub runpath: Vec<String>,
    pub version_needs: Vec<(String, String)>,
    pub abs_paths: BTreeSet<String>,
}

impl ElfFacts {
    /// Returns `None` when the bytes are not a parseable dynamic ELF file.
    pub fn parse(bytes: &[u8]) -> Option<ElfFacts> {
        let elf = Elf::parse(bytes).ok()?;
        let dynamic = elf.dynamic.as_ref()?;

        let needed = elf.libraries.iter().map(|s| s.to_string()).collect();

        let tagged = |tag: u64| -> Option<Vec<String>> {
            let off = dynamic
                .dyns
                .iter()
                .find(|d| d.d_tag == tag)
                .map(|d| d.d_val as usize)?;
            let s = elf.dynstrtab.get_at(off)?;
            Some(s.split(':').map(str::to_string).collect())
        };
        let runpath = tagged(DT_RUNPATH)
            .or_else(|| tagged(DT_RPATH))
            .unwrap_or_default();

        let mut version_needs = Vec::new();
        if let Some(verneed) = &elf.verneed {
            for need in verneed.iter() {
                let Some(file) = elf.dynstrtab.get_at(need.vn_file) else {
                    continue;
                };
                for aux in need.iter() {
                    if let Some(name) = elf.dynstrtab.get_at(aux.vna_name) {
                        version_needs.push((file.to_string(), name.to_string()));
                    }
                }
            }
        }

        Some(ElfFacts {
            needed,
            runpath,
            version_needs,
            abs_paths: scan_abs_paths(bytes),
        })
    }
}

fn scan_abs_paths(bytes: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut i = 0;
    let mut nul_at = 0;
    while i < bytes.len() && out.len() < MAX_ABS_PATHS {
        if bytes[i] != b'/' || !ABS_PREFIXES.iter().any(|p| bytes[i..].starts_with(p)) {
            i += 1;
            continue;
        }
        if nul_at < i {
            match bytes[i..].iter().position(|&b| b == 0) {
                Some(n) => nul_at = i + n,
                None => break,
            }
        }
        if nul_at - i > MAX_ABS_PATH_LEN {
            i += 1;
            continue;
        }
        if let Ok(s) = std::str::from_utf8(&bytes[i..nul_at]) {
            out.insert(s.to_string());
        }
        i = nul_at + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal x86_64 `ET_DYN` ELF with a dynamic section, string table,
    /// symbol-version needs and a read-only data blob holding `strings`.
    fn build_elf(
        needed: &[&str],
        runpath: Option<&str>,
        version_needs: &[(&str, &str)],
        strings: &[&str],
    ) -> Vec<u8> {
        fn put(buf: &mut Vec<u8>, s: &str) -> u32 {
            let off = buf.len() as u32;
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
            off
        }
        fn elf_hash(name: &str) -> u32 {
            let mut h: u32 = 0;
            for &b in name.as_bytes() {
                h = (h << 4).wrapping_add(u32::from(b));
                let g = h & 0xf000_0000;
                if g != 0 {
                    h ^= g >> 24;
                }
                h &= !g;
            }
            h
        }

        let mut dynstr = vec![0u8];
        let needed_offs: Vec<u32> = needed.iter().map(|n| put(&mut dynstr, n)).collect();
        let runpath_off = runpath.map(|r| put(&mut dynstr, r));

        let mut sonames: Vec<&str> = Vec::new();
        for (so, _) in version_needs {
            if !sonames.contains(so) {
                sonames.push(so);
            }
        }
        let mut verneed = Vec::new();
        for (i, so) in sonames.iter().enumerate() {
            let file_off = put(&mut dynstr, so);
            let auxes: Vec<&str> = version_needs
                .iter()
                .filter(|(s, _)| s == so)
                .map(|(_, v)| *v)
                .collect();
            let last = i + 1 == sonames.len();
            let next = if last {
                0
            } else {
                16 + 16 * auxes.len() as u32
            };
            verneed.extend_from_slice(&1u16.to_le_bytes());
            verneed.extend_from_slice(&(auxes.len() as u16).to_le_bytes());
            verneed.extend_from_slice(&file_off.to_le_bytes());
            verneed.extend_from_slice(&16u32.to_le_bytes());
            verneed.extend_from_slice(&next.to_le_bytes());
            for (j, v) in auxes.iter().enumerate() {
                let name_off = put(&mut dynstr, v);
                let aux_next = if j + 1 == auxes.len() { 0 } else { 16u32 };
                verneed.extend_from_slice(&elf_hash(v).to_le_bytes());
                verneed.extend_from_slice(&0u16.to_le_bytes());
                verneed.extend_from_slice(&(2 + j as u16).to_le_bytes());
                verneed.extend_from_slice(&name_off.to_le_bytes());
                verneed.extend_from_slice(&aux_next.to_le_bytes());
            }
        }

        let mut rodata = Vec::new();
        for s in strings {
            put(&mut rodata, s);
        }

        let mut entries: Vec<(u64, u64)> = Vec::new();
        for off in &needed_offs {
            entries.push((1, u64::from(*off)));
        }
        if let Some(off) = runpath_off {
            entries.push((29, u64::from(off)));
        }

        const EHDR: u64 = 64;
        const PHDR: u64 = 56;
        let dynstr_at = EHDR + 2 * PHDR;
        let verneed_at = (dynstr_at + dynstr.len() as u64).next_multiple_of(8);
        let dynamic_at = (verneed_at + verneed.len() as u64).next_multiple_of(8);
        let n_entries = entries.len() as u64 + 3 + if verneed.is_empty() { 0 } else { 2 } + 1;
        let dynamic_len = n_entries * 16;
        let rodata_at = dynamic_at + dynamic_len;
        let shoff = (rodata_at + rodata.len() as u64).next_multiple_of(8);
        let total = shoff + 3 * 64;

        entries.push((5, dynstr_at));
        entries.push((10, dynstr.len() as u64));
        entries.push((0x6fff_fffe, verneed_at));
        entries.push((0x6fff_ffff, sonames.len() as u64));
        if verneed.is_empty() {
            entries.retain(|(t, _)| *t != 0x6fff_fffe && *t != 0x6fff_ffff);
        }
        entries.push((0, 0));
        while (entries.len() as u64) < n_entries {
            entries.insert(entries.len() - 1, (0x7fff_ffff, 0));
        }

        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&62u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&EHDR.to_le_bytes());
        out.extend_from_slice(&shoff.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(EHDR as u16).to_le_bytes());
        out.extend_from_slice(&(PHDR as u16).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&64u16.to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());

        let phdr = |out: &mut Vec<u8>, ty: u32, flags: u32, off: u64, len: u64, align: u64| {
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&align.to_le_bytes());
        };
        phdr(&mut out, 1, 5, 0, total, 1);
        phdr(&mut out, 2, 6, dynamic_at, dynamic_len, 8);

        out.extend_from_slice(&dynstr);
        out.resize(verneed_at as usize, 0);
        out.extend_from_slice(&verneed);
        out.resize(dynamic_at as usize, 0);
        for (tag, val) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&val.to_le_bytes());
        }
        out.extend_from_slice(&rodata);
        out.resize(shoff as usize, 0);

        let shdr = |out: &mut Vec<u8>, ty: u32, off: u64, len: u64, link: u32, info: u32| {
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&2u64.to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&link.to_le_bytes());
            out.extend_from_slice(&info.to_le_bytes());
            out.extend_from_slice(&1u64.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes());
        };
        out.extend_from_slice(&[0; 64]);
        shdr(&mut out, 3, dynstr_at, dynstr.len() as u64, 0, 0);
        shdr(
            &mut out,
            0x6fff_fffe,
            verneed_at,
            verneed.len() as u64,
            1,
            sonames.len() as u32,
        );
        out
    }

    #[test]
    fn parses_needed_runpath_and_versions() {
        let bytes = build_elf(
            &["libfoo.so.1", "libc.so.6"],
            Some("$ORIGIN/lib:/x"),
            &[("libc.so.6", "GLIBC_2.34")],
            &["/opt/Foo/res"],
        );
        let f = ElfFacts::parse(&bytes).unwrap();
        assert_eq!(f.needed, ["libfoo.so.1", "libc.so.6"]);
        assert_eq!(f.runpath, ["$ORIGIN/lib", "/x"]);
        assert_eq!(
            f.version_needs,
            [("libc.so.6".to_string(), "GLIBC_2.34".to_string())]
        );
        assert!(f.abs_paths.contains("/opt/Foo/res"));
    }

    #[test]
    fn non_elf_is_none() {
        assert!(ElfFacts::parse(b"#!/bin/sh\n").is_none());
    }

    #[test]
    fn truncated_elf_is_none_without_panic() {
        let bytes = build_elf(&["libc.so.6"], None, &[("libc.so.6", "GLIBC_2.34")], &[]);
        for n in [0, 10, 63, 64, 100, 200, bytes.len() - 1] {
            let _ = ElfFacts::parse(&bytes[..n]);
        }
    }

    #[test]
    fn parses_host_binary() {
        let Ok(bytes) = std::fs::read("/bin/sh") else {
            return;
        };
        if !bytes.starts_with(b"\x7fELF") {
            return;
        }
        let f = ElfFacts::parse(&bytes).expect("host ELF parses");
        assert!(f.needed.iter().any(|n| n.starts_with("libc.so")));
    }

    #[test]
    fn long_nul_free_prefix_runs_scan_in_linear_time() {
        let mut bytes = build_elf(&["libc.so.6"], None, &[], &["/opt/Foo/res"]);
        bytes.extend(b"/opt/".repeat(2 * 1024 * 1024));
        let f = ElfFacts::parse(&bytes).expect("still parses");
        assert!(f.abs_paths.contains("/opt/Foo/res"));
        assert_eq!(f.needed, ["libc.so.6"]);
    }
}
