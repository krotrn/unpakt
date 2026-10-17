use regex::Regex;

use crate::error::{Error, Result};
use crate::host::Host;
use crate::model::{Arch, Asset};

const TARBALL_SUFFIXES: [&str; 8] = [
    ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tar.bz2", ".tbz2", ".tar",
];

/// Checksum and signature companions, plus extensions of other platforms' installers.
const REJECTED_SUFFIXES: [&str; 13] = [
    ".sha256",
    ".sha256sum",
    ".sha512",
    ".sig",
    ".asc",
    ".minisig",
    ".zsync",
    ".exe",
    ".msi",
    ".dmg",
    ".apk",
    ".pkg.tar.zst",
    ".pkg.tar.xz",
];

const REJECTED_NAME_PARTS: [&str; 2] = ["sha256sums", "checksums"];

const OTHER_OS_TOKENS: [&str; 9] = [
    "windows", "win32", "win64", "darwin", "macos", "osx", "apple", "android", "freebsd",
];

const OTHER_ARCH_TOKENS: &[&str] = &[
    "i386",
    "i586",
    "i686",
    "x86",
    "arm",
    "armel",
    "armhf",
    "armv6",
    "armv6l",
    "armv7",
    "armv7l",
    "ppc",
    "ppc64",
    "ppc64le",
    "powerpc",
    "s390x",
    "riscv64",
    "loongarch64",
    "mips",
    "mipsel",
    "mips64",
    "mips64el",
    "sparc",
    "sparc64",
];

/// Scores an asset name for `host`, or returns `None` when the name must be rejected.
/// Rejected names are checksums, other OSes or architectures, unrecognized formats,
/// and names that do not match `pattern`.
pub fn score_asset(name: &str, host: &Host, pattern: Option<&Regex>) -> Option<i32> {
    if pattern.is_some_and(|re| !re.is_match(name)) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if REJECTED_SUFFIXES.iter().any(|s| lower.ends_with(s))
        || REJECTED_NAME_PARTS.iter().any(|p| lower.contains(p))
    {
        return None;
    }
    let tokens = tokens(&lower);
    if tokens.iter().any(|t| OTHER_OS_TOKENS.contains(&t.as_str()))
        || has_foreign_arch(&tokens, &host.arch)
    {
        return None;
    }

    let mut score = if lower.ends_with(".appimage") {
        30
    } else if TARBALL_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        20
    } else {
        return None;
    };
    if host
        .arch
        .aliases()
        .iter()
        .any(|alias| tokens.iter().any(|t| t.as_str() == *alias))
    {
        score += 40;
    }
    if tokens.iter().any(|t| t == "linux") {
        score += 10;
    }
    Some(score)
}

/// The best-scoring asset for `host`. Ties keep the earliest asset.
pub fn choose<'a>(assets: &'a [Asset], host: &Host, pattern: Option<&Regex>) -> Result<&'a Asset> {
    let mut best: Option<(i32, &Asset)> = None;
    for asset in assets {
        let Some(score) = score_asset(&asset.name, host, pattern) else {
            continue;
        };
        if best.is_none_or(|(top, _)| score > top) {
            best = Some((score, asset));
        }
    }
    best.map(|(_, asset)| asset).ok_or_else(|| {
        let names: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
        Error::NotFound(format!(
            "no release asset matches this host: {}",
            names.join(", ")
        ))
    })
}

/// Splits a lowercased name on non-alphanumeric characters, rejoining `x86` `64` into `x86_64`.
fn tokens(lower: &str) -> Vec<String> {
    let raw: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == "x86" && raw.get(i + 1) == Some(&"64") {
            out.push("x86_64".to_string());
            i += 2;
        } else {
            out.push(raw[i].to_string());
            i += 1;
        }
    }
    out
}

/// True when a token names an architecture other than the host's.
fn has_foreign_arch(tokens: &[String], host_arch: &Arch) -> bool {
    let host_aliases = host_arch.aliases();
    let host_name = match host_arch {
        Arch::Other(name) => name.as_str(),
        _ => "",
    };
    tokens.iter().any(|t| {
        let t = t.as_str();
        if t == host_name || host_aliases.contains(&t) {
            return false;
        }
        [Arch::X86_64, Arch::Aarch64]
            .iter()
            .any(|arch| arch.aliases().contains(&t))
            || OTHER_ARCH_TOKENS.contains(&t)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::model::Arch;
    use regex::Regex;

    fn assets(names: &[&str]) -> Vec<Asset> {
        names
            .iter()
            .map(|name| Asset {
                name: name.to_string(),
                url: format!("https://example.invalid/{name}"),
                size: None,
            })
            .collect()
    }

    fn x86_host() -> Host {
        Host {
            arch: Arch::X86_64,
            os_id: None,
            os_like: vec![],
            immutable_root: false,
            fuse2: false,
        }
    }

    #[test]
    fn chooses_assets() {
        // host x86_64
        let host = x86_host();
        let pick = |names: &[&str], pat: Option<&Regex>| {
            choose(&assets(names), &host, pat).map(|a| a.name.clone())
        };
        assert_eq!(
            pick(
                &[
                    "app-1.0-x86_64.AppImage",
                    "app-1.0-aarch64.AppImage",
                    "app-1.0-x86_64.tar.gz",
                    "SHA256SUMS"
                ],
                None
            )
            .unwrap(),
            "app-1.0-x86_64.AppImage"
        );
        assert_eq!(
            pick(
                &[
                    "tool-linux-amd64.tar.gz",
                    "tool-darwin-amd64.tar.gz",
                    "tool-windows-amd64.zip"
                ],
                None
            )
            .unwrap(),
            "tool-linux-amd64.tar.gz"
        );
        assert_eq!(
            pick(
                &[
                    "nvim-linux-x86_64.tar.gz",
                    "nvim-linux-x86_64.appimage",
                    "nvim-linux-arm64.tar.gz",
                    "nvim-linux-x86_64.appimage.zsync"
                ],
                None
            )
            .unwrap(),
            "nvim-linux-x86_64.appimage"
        );
        assert_eq!(
            pick(
                &["a-x86_64.AppImage", "a-x86_64.tar.gz"],
                Some(&Regex::new("tar").unwrap())
            )
            .unwrap(),
            "a-x86_64.tar.gz"
        );
        assert!(matches!(
            pick(&["a.exe", "a.dmg"], None),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn rejects_foreign_and_unrecognized_assets() {
        let host = x86_host();
        for name in [
            "app-aarch64.AppImage",
            "app-arm64.tar.gz",
            "app-i686.tar.gz",
            "app-x86_64.zsync",
            "app-x86_64.pkg.tar.zst",
            "app-x86_64.AppImage.sha256",
            "app-x86_64.deb",
            "app-x86_64.zip",
            "checksums.txt",
        ] {
            assert_eq!(score_asset(name, &host, None), None, "{name}");
        }
    }

    #[test]
    fn foreign_arch_without_native_asset_is_not_found() {
        let host = x86_host();
        for name in [
            "tool-linux-ppc64le.AppImage",
            "tool-linux-s390x.tar.gz",
            "tool-linux-x86.tar.gz",
            "tool-riscv64.tar.gz",
            "tool-armv7l.tar.gz",
            "pkg-1.0-1-x86_64.pkg.tar.xz",
        ] {
            let list = assets(&[name]);
            let result = choose(&list, &host, None);
            assert!(matches!(result, Err(Error::NotFound(_))), "{name}");
        }
        assert_eq!(score_asset("tool-x86_64.tar.gz", &host, None), Some(60));
    }

    #[test]
    fn arch_tokens_match_on_boundaries_only() {
        let host = x86_host();
        assert_eq!(score_asset("app-x64.tar.gz", &host, None), Some(60));
        assert_eq!(score_asset("APP-X86-64.TAR.GZ", &host, None), Some(60));
        assert_eq!(score_asset("app-ax64b.tar.gz", &host, None), Some(20));
    }
}
