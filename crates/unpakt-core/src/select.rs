use regex::Regex;

use crate::error::{Error, Result};
use crate::host::Host;
use crate::model::{Arch, Asset, FormatKind};

const TARBALL_SUFFIXES: [&str; 8] = [
    ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tar.bz2", ".tbz2", ".tar",
];

/// Checksum and signature companions, plus extensions of other platforms' installers.
const REJECTED_SUFFIXES: [&str; 11] = [
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
    "ppc64el",
    "armv7hl",
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

/// The format an asset name denotes, or `None` for anything unrecognized
/// (including source rpms).
pub fn asset_format(name: &str) -> Option<FormatKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".appimage") {
        Some(FormatKind::AppImage)
    } else if lower.ends_with(".pkg.tar.zst") || lower.ends_with(".pkg.tar.xz") {
        Some(FormatKind::Pacman)
    } else if lower.ends_with(".src.rpm") {
        None
    } else if lower.ends_with(".rpm") {
        Some(FormatKind::Rpm)
    } else if lower.ends_with(".deb") {
        Some(FormatKind::Deb)
    } else if TARBALL_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        Some(FormatKind::Tarball)
    } else {
        None
    }
}

/// Scores an asset name for `host`, or returns `None` when the name must be rejected.
/// Rejected names are checksums, other OSes or architectures, unrecognized formats,
/// formats other than `format`, and names that do not match `pattern`.
pub fn score_asset(
    name: &str,
    host: &Host,
    pattern: Option<&Regex>,
    format: Option<FormatKind>,
) -> Option<i32> {
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

    let kind = asset_format(name)?;
    if format.is_some_and(|f| f != kind) {
        return None;
    }
    let mut score = match kind {
        FormatKind::AppImage => 30,
        FormatKind::Tarball => 20,
        _ if kind.family() == host.family() => 15,
        _ => 10,
    };
    // Packages carry their architecture in metadata and foreign ones are already
    // rejected above, so an arch token must not lift a package over an AppImage
    // or tarball.
    let is_package = kind.family().is_some();
    if !is_package
        && host
            .arch
            .aliases()
            .iter()
            .any(|alias| tokens.iter().any(|t| t.as_str() == *alias))
    {
        score += 40;
    }
    if !is_package && tokens.iter().any(|t| t == "linux") {
        score += 10;
    }
    Some(score)
}

/// The best-scoring asset for `host`. Ties keep the earliest asset.
pub fn choose<'a>(
    assets: &'a [Asset],
    host: &Host,
    pattern: Option<&Regex>,
    format: Option<FormatKind>,
) -> Result<&'a Asset> {
    let mut best: Option<(i32, &Asset)> = None;
    for asset in assets {
        let Some(score) = score_asset(&asset.name, host, pattern, format) else {
            continue;
        };
        if best.is_none_or(|(top, _)| score > top) {
            best = Some((score, asset));
        }
    }
    best.map(|(_, asset)| asset).ok_or_else(|| {
        let names = assets
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        match format {
            Some(f) => Error::NotFound(format!("no {} asset in this release: {names}", f.as_str())),
            None => Error::NotFound(format!("no release asset matches this host: {names}")),
        }
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

    fn host_of(os_id: &str) -> Host {
        Host {
            os_id: Some(os_id.to_string()),
            ..x86_host()
        }
    }

    #[test]
    fn chooses_package_assets() {
        let pick = |host: &Host, names: &[&str]| {
            choose(&assets(names), host, None, None).map(|a| a.name.clone())
        };
        let arch = host_of("arch");
        let debrpm = ["app_1.0_amd64.deb", "app-1.0.x86_64.rpm"];
        assert_eq!(
            pick(&arch, &["app.AppImage", "app_1.0_amd64.deb"]).unwrap(),
            "app.AppImage"
        );
        assert_eq!(pick(&arch, &debrpm).unwrap(), "app_1.0_amd64.deb");
        assert_eq!(
            pick(&host_of("fedora"), &debrpm).unwrap(),
            "app-1.0.x86_64.rpm"
        );
        assert_eq!(
            pick(
                &host_of("ubuntu"),
                &["app-1.0.x86_64.rpm", "app_1.0_amd64.deb"]
            )
            .unwrap(),
            "app_1.0_amd64.deb"
        );
        assert!(matches!(
            pick(&arch, &["app-1.0.src.rpm"]),
            Err(Error::NotFound(_))
        ));
        assert_eq!(
            pick(&arch, &["app_1.0_arm64.deb", "app_1.0_amd64.deb"]).unwrap(),
            "app_1.0_amd64.deb"
        );
        assert_eq!(
            pick(&arch, &["app-1.0-1-x86_64.pkg.tar.zst", "app.tar.gz"]).unwrap(),
            "app.tar.gz"
        );
        assert_eq!(
            pick(&arch, &["app-1.0-1-any.pkg.tar.zst"]).unwrap(),
            "app-1.0-1-any.pkg.tar.zst"
        );
    }

    #[test]
    fn linux_token_does_not_lift_packages() {
        let host = host_of("ubuntu");
        let list = assets(&["app-linux.deb", "app.tar.gz"]);
        assert_eq!(choose(&list, &host, None, None).unwrap().name, "app.tar.gz");
    }

    #[test]
    fn foreign_arch_packages_are_rejected() {
        let host = host_of("ubuntu");
        for name in [
            "app_1.0_armhf.deb",
            "app_1.0_armel.deb",
            "app_1.0_i386.deb",
            "app_1.0_ppc64el.deb",
            "app-1.0.s390x.rpm",
            "app-1.0.armv7hl.rpm",
            "app-1.0.i686.rpm",
            "app-1.0.riscv64.rpm",
        ] {
            assert_eq!(score_asset(name, &host, None, None), None, "{name}");
        }
    }

    #[test]
    fn equal_packages_keep_the_earliest() {
        let list = assets(&["app-1.0.x86_64.rpm", "app_1.0_amd64.deb"]);
        assert_eq!(
            choose(&list, &host_of("arch"), None, None).unwrap().name,
            "app-1.0.x86_64.rpm"
        );
    }

    #[test]
    fn format_filter_restricts() {
        let host = host_of("arch");
        let list = assets(&["app.AppImage", "app.tar.gz"]);
        assert_eq!(
            choose(&list, &host, None, Some(FormatKind::Tarball))
                .unwrap()
                .name,
            "app.tar.gz"
        );
        match choose(&list, &host, None, Some(FormatKind::Deb)) {
            Err(Error::NotFound(m)) => assert!(m.contains("deb"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn asset_format_names() {
        assert_eq!(asset_format("A.AppImage"), Some(FormatKind::AppImage));
        assert_eq!(asset_format("a.pkg.tar.zst"), Some(FormatKind::Pacman));
        assert_eq!(asset_format("a.pkg.tar.xz"), Some(FormatKind::Pacman));
        assert_eq!(asset_format("a.src.rpm"), None);
        assert_eq!(asset_format("a.rpm"), Some(FormatKind::Rpm));
        assert_eq!(asset_format("a.deb"), Some(FormatKind::Deb));
        assert_eq!(asset_format("a.tar.gz"), Some(FormatKind::Tarball));
        assert_eq!(asset_format("a.zip"), None);
    }

    #[test]
    fn chooses_assets() {
        // host x86_64
        let host = x86_host();
        let pick = |names: &[&str], pat: Option<&Regex>| {
            choose(&assets(names), &host, pat, None).map(|a| a.name.clone())
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
            "app-x86_64.AppImage.sha256",
            "app-x86_64.src.rpm",
            "app-x86_64.zip",
            "checksums.txt",
        ] {
            assert_eq!(score_asset(name, &host, None, None), None, "{name}");
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
            "pkg-1.0-1-aarch64.pkg.tar.xz",
        ] {
            let list = assets(&[name]);
            let result = choose(&list, &host, None, None);
            assert!(matches!(result, Err(Error::NotFound(_))), "{name}");
        }
        assert_eq!(
            score_asset("tool-x86_64.tar.gz", &host, None, None),
            Some(60)
        );
    }

    #[test]
    fn arch_tokens_match_on_boundaries_only() {
        let host = x86_host();
        assert_eq!(score_asset("app-x64.tar.gz", &host, None, None), Some(60));
        assert_eq!(
            score_asset("APP-X86-64.TAR.GZ", &host, None, None),
            Some(60)
        );
        assert_eq!(score_asset("app-ax64b.tar.gz", &host, None, None), Some(20));
    }
}
