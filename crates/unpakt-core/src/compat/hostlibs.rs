use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use goblin::elf::Elf;

use crate::model::Arch;

const NEW_MAGIC: &[u8] = b"glibc-ld.so.cache1.1";
const OLD_MAGIC: &[u8] = b"ld.so-1.7.0";
const NEW_HEADER_LEN: usize = 48;
const NEW_ENTRY_LEN: usize = 24;
const FLAG_ELF_LIBC6: u32 = 0x0003;
const MAX_LIB_BYTES: u64 = 64 * 1024 * 1024;

const FALLBACK_DIRS: [&str; 8] = [
    "/lib64",
    "/usr/lib64",
    "/lib",
    "/usr/lib",
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
];

/// The shared libraries the host provides, their symbol versions, and whether
/// unprivileged user namespaces are available.
#[derive(Debug)]
pub struct HostLibs {
    sonames: BTreeMap<String, PathBuf>,
    versions: RefCell<BTreeMap<String, Option<BTreeSet<String>>>>,
    userns: bool,
}

impl HostLibs {
    /// Probes the running system. Unreadable sources count as absent, so this never fails.
    pub fn probe() -> HostLibs {
        let sonames = std::fs::read("/etc/ld.so.cache")
            .ok()
            .and_then(|bytes| parse_ld_so_cache(&bytes, &Arch::current()))
            .unwrap_or_else(list_fallback_dirs);
        HostLibs {
            sonames,
            versions: RefCell::new(BTreeMap::new()),
            userns: probe_userns(),
        }
    }

    pub fn from_parts(
        sonames: BTreeMap<String, PathBuf>,
        versions: BTreeMap<String, BTreeSet<String>>,
        userns: bool,
    ) -> HostLibs {
        let cache = versions
            .into_iter()
            .map(|(soname, set)| (soname, Some(set)))
            .collect();
        HostLibs {
            sonames,
            versions: RefCell::new(cache),
            userns,
        }
    }

    pub fn has(&self, soname: &str) -> bool {
        self.sonames.contains_key(soname)
    }

    /// Whether the host's `soname` defines symbol `version`. `None` means the
    /// version set could not be read.
    pub fn provides(&self, soname: &str, version: &str) -> Option<bool> {
        if let Some(cached) = self.versions.borrow().get(soname) {
            return cached.as_ref().map(|set| set.contains(version));
        }
        let set = self.sonames.get(soname).and_then(|path| read_verdef(path));
        let answer = set.as_ref().map(|s| s.contains(version));
        self.versions.borrow_mut().insert(soname.to_string(), set);
        answer
    }

    pub fn userns(&self) -> bool {
        self.userns
    }
}

fn read_verdef(path: &Path) -> Option<BTreeSet<String>> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_LIB_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let elf = Elf::parse(&bytes).ok()?;
    let verdef = elf.verdef.as_ref()?;
    let mut set = BTreeSet::new();
    for def in verdef.iter() {
        for aux in def.iter() {
            if let Some(name) = elf.dynstrtab.get_at(aux.vda_name) {
                set.insert(name.to_string());
            }
        }
    }
    Some(set)
}

fn list_fallback_dirs() -> BTreeMap<String, PathBuf> {
    let mut map = BTreeMap::new();
    for dir in FALLBACK_DIRS {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if (name.starts_with("lib") || name.starts_with("ld-linux")) && name.contains(".so") {
                map.entry(name.to_string()).or_insert_with(|| entry.path());
            }
        }
    }
    map
}

fn read_flag(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn probe_userns() -> bool {
    userns_allowed(
        read_flag("/proc/sys/kernel/unprivileged_userns_clone").as_deref(),
        read_flag("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref(),
        read_flag("/proc/sys/user/max_user_namespaces").as_deref(),
    )
}

fn userns_allowed(clone: Option<&str>, apparmor_restrict: Option<&str>, max: Option<&str>) -> bool {
    clone != Some("0") && apparmor_restrict != Some("1") && max != Some("0")
}

fn u32_at(bytes: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(off..end)?.try_into().ok()?))
}

fn cstr_at(bytes: &[u8], off: usize) -> Option<&str> {
    let rest = bytes.get(off..)?;
    let len = rest.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&rest[..len]).ok()
}

/// Reads the `glibc-ld.so.cache1.1` format, at the start of `bytes` or after
/// an old-format block. Maps each soname to the path the loader would use for
/// `arch`: on x86-64 and aarch64 only entries tagged with that architecture
/// count, so 32-bit libraries (which carry no architecture bits) are ignored.
pub(crate) fn parse_ld_so_cache(bytes: &[u8], arch: &Arch) -> Option<BTreeMap<String, PathBuf>> {
    let base = find_new_header(bytes)?;
    let header = bytes.get(base..)?;
    let nlibs = u32_at(header, 20)? as usize;
    let entries_len = nlibs.checked_mul(NEW_ENTRY_LEN)?;
    let entries_end = NEW_HEADER_LEN.checked_add(entries_len)?;
    if entries_end > header.len() {
        return None;
    }
    let want = arch_bits(arch);
    let mut best: BTreeMap<String, (u8, PathBuf)> = BTreeMap::new();
    for i in 0..nlibs {
        let at = NEW_HEADER_LEN + i * NEW_ENTRY_LEN;
        let flags = u32_at(header, at)?;
        if flags & 0xff != FLAG_ELF_LIBC6 {
            continue;
        }
        let bits = flags & 0xff00;
        let rank = match want {
            Some(w) if bits == w => 0,
            Some(_) => continue,
            None if bits == 0 => 1,
            None => continue,
        };
        let key = u32_at(header, at + 4)? as usize;
        let value = u32_at(header, at + 8)? as usize;
        let (Some(name), Some(path)) = (cstr_at(header, key), cstr_at(header, value)) else {
            continue;
        };
        match best.get(name) {
            Some((have, _)) if *have <= rank => {}
            _ => {
                best.insert(name.to_string(), (rank, PathBuf::from(path)));
            }
        }
    }
    Some(best.into_iter().map(|(k, (_, v))| (k, v)).collect())
}

fn arch_bits(arch: &Arch) -> Option<u32> {
    match arch {
        Arch::X86_64 => Some(0x0300),
        Arch::Aarch64 => Some(0x0a00),
        Arch::Other(_) => None,
    }
}

fn find_new_header(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(NEW_MAGIC) {
        return Some(0);
    }
    if !bytes.starts_with(OLD_MAGIC) {
        return None;
    }
    let nlibs = u32_at(bytes, 12)? as usize;
    let after_entries = 16usize.checked_add(nlibs.checked_mul(12)?)?;
    let mut off = after_entries.checked_add(7)? & !7;
    while off.checked_add(NEW_MAGIC.len())? <= bytes.len() {
        if bytes[off..].starts_with(NEW_MAGIC) {
            return Some(off);
        }
        off += 8;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_entries(old_block: bool, entries: &[(u32, &str, &str)]) -> Vec<u8> {
        let mut strings = Vec::new();
        let mut offs = Vec::new();
        for (_, name, path) in entries {
            let k = strings.len() as u32;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            let v = strings.len() as u32;
            strings.extend_from_slice(path.as_bytes());
            strings.push(0);
            offs.push((k, v));
        }
        let strings_start = (NEW_HEADER_LEN + entries.len() * NEW_ENTRY_LEN) as u32;
        let mut new = Vec::new();
        new.extend_from_slice(NEW_MAGIC);
        new.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        new.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        new.extend_from_slice(&[0u8; 4]);
        new.extend_from_slice(&[0u8; 16]);
        assert_eq!(new.len(), NEW_HEADER_LEN);
        for ((flags, _, _), (k, v)) in entries.iter().zip(offs) {
            new.extend_from_slice(&flags.to_le_bytes());
            new.extend_from_slice(&(strings_start + k).to_le_bytes());
            new.extend_from_slice(&(strings_start + v).to_le_bytes());
            new.extend_from_slice(&0u32.to_le_bytes());
            new.extend_from_slice(&0u64.to_le_bytes());
        }
        new.extend_from_slice(&strings);
        if !old_block {
            return new;
        }
        let mut out = Vec::new();
        out.extend_from_slice(OLD_MAGIC);
        out.push(0);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        while out.len() % 8 != 0 {
            out.push(0);
        }
        out.extend_from_slice(&new);
        out
    }

    fn build_cache(old_block: bool) -> Vec<u8> {
        build_entries(
            old_block,
            &[
                (0x0303, "libfoo.so.1", "/usr/lib/libfoo.so.1"),
                (0x0303, "libbar.so.2", "/usr/lib/libbar.so.2"),
            ],
        )
    }

    #[test]
    fn picks_host_arch_entry_in_either_order() {
        let i386 = (0x0003, "libx.so.1", "/usr/lib32/libx.so.1");
        let x64 = (0x0303, "libx.so.1", "/usr/lib/libx.so.1");
        for entries in [[i386, x64], [x64, i386]] {
            let map = parse_ld_so_cache(&build_entries(false, &entries), &Arch::X86_64).unwrap();
            assert_eq!(map["libx.so.1"], PathBuf::from("/usr/lib/libx.so.1"));
        }
    }

    #[test]
    fn other_arch_and_untagged_entries_are_absent() {
        let bytes = build_entries(false, &[(0x0003, "libz.so.1", "/usr/lib32/libz.so.1")]);
        let map = parse_ld_so_cache(&bytes, &Arch::X86_64).unwrap();
        assert!(!map.contains_key("libz.so.1"));

        let bytes = build_entries(false, &[(0x0a03, "liby.so.1", "/usr/lib/liby.so.1")]);
        let map = parse_ld_so_cache(&bytes, &Arch::X86_64).unwrap();
        assert!(!map.contains_key("liby.so.1"));
        let map = parse_ld_so_cache(&bytes, &Arch::Aarch64).unwrap();
        assert!(map.contains_key("liby.so.1"));
    }

    #[test]
    fn parses_ld_so_cache_new_format() {
        for old in [false, true] {
            let map = parse_ld_so_cache(&build_cache(old), &Arch::X86_64).expect("parsed");
            assert_eq!(
                map.get("libfoo.so.1"),
                Some(&PathBuf::from("/usr/lib/libfoo.so.1"))
            );
            assert_eq!(map.len(), 2);
        }
    }

    #[test]
    fn parses_real_cache_when_present() {
        let Ok(bytes) = std::fs::read("/etc/ld.so.cache") else {
            return;
        };
        let map = parse_ld_so_cache(&bytes, &Arch::current()).expect("real cache parses");
        assert!(map.keys().any(|k| k.starts_with("libc.so")));
    }

    #[test]
    fn garbage_cache_is_none() {
        assert!(parse_ld_so_cache(b"", &Arch::X86_64).is_none());
        assert!(parse_ld_so_cache(b"not a cache at all, really", &Arch::X86_64).is_none());
        let mut truncated = build_cache(false);
        truncated.truncate(60);
        assert!(parse_ld_so_cache(&truncated, &Arch::X86_64).is_none());
        let mut lying = build_cache(false);
        lying[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_ld_so_cache(&lying, &Arch::X86_64).is_none());
    }

    #[test]
    fn from_parts_answers() {
        let sonames = BTreeMap::from([("libc.so.6".to_string(), PathBuf::from("/lib/libc.so.6"))]);
        let versions = BTreeMap::from([(
            "libc.so.6".to_string(),
            BTreeSet::from(["GLIBC_2.17".to_string()]),
        )]);
        let libs = HostLibs::from_parts(sonames, versions, true);
        assert!(libs.has("libc.so.6"));
        assert!(!libs.has("libnope.so"));
        assert_eq!(libs.provides("libc.so.6", "GLIBC_2.17"), Some(true));
        assert_eq!(libs.provides("libc.so.6", "GLIBC_9.99"), Some(false));
        assert_eq!(libs.provides("libnope.so", "X"), None);
        assert!(libs.userns());
    }

    #[test]
    fn probe_reads_real_libc_versions() {
        let libs = HostLibs::probe();
        if libs.has("libc.so.6") {
            let version = match Arch::current() {
                Arch::Aarch64 => "GLIBC_2.17",
                _ => "GLIBC_2.2.5",
            };
            assert_eq!(libs.provides("libc.so.6", version), Some(true));
        }
    }

    #[test]
    fn userns_decision() {
        assert!(userns_allowed(None, None, None));
        assert!(!userns_allowed(Some("0"), None, None));
        assert!(!userns_allowed(None, Some("1"), None));
        assert!(!userns_allowed(None, None, Some("0")));
        assert!(userns_allowed(Some("1"), Some("0"), Some("1000")));
    }
}
