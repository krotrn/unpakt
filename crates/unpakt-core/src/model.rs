use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use crate::compat::elf::ElfFacts;
use crate::error::Error;
use crate::format::stream::DataSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatKind {
    AppImage,
    Tarball,
    Deb,
    Rpm,
    Pacman,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageFamily {
    Deb,
    Rpm,
    Pacman,
}

impl FormatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FormatKind::AppImage => "appimage",
            FormatKind::Tarball => "tarball",
            FormatKind::Deb => "deb",
            FormatKind::Rpm => "rpm",
            FormatKind::Pacman => "pacman",
        }
    }

    pub fn family(self) -> Option<PackageFamily> {
        match self {
            FormatKind::Deb => Some(PackageFamily::Deb),
            FormatKind::Rpm => Some(PackageFamily::Rpm),
            FormatKind::Pacman => Some(PackageFamily::Pacman),
            FormatKind::AppImage | FormatKind::Tarball => None,
        }
    }
}

impl FromStr for FormatKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "appimage" => Ok(FormatKind::AppImage),
            "tarball" => Ok(FormatKind::Tarball),
            "deb" => Ok(FormatKind::Deb),
            "rpm" => Ok(FormatKind::Rpm),
            "pacman" => Ok(FormatKind::Pacman),
            other => Err(Error::Parse(format!("unknown format kind: {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
    Other(String),
}

impl Arch {
    pub fn current() -> Arch {
        match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            other => Arch::Other(other.to_string()),
        }
    }

    /// Spellings that release file names use for this architecture.
    pub fn aliases(&self) -> &[&str] {
        match self {
            Arch::X86_64 => &["x86_64", "amd64", "x64"],
            Arch::Aarch64 => &["aarch64", "arm64"],
            Arch::Other(_) => &[],
        }
    }

    /// Spellings that package metadata uses for this architecture.
    pub fn package_aliases(&self) -> &[&str] {
        match self {
            Arch::X86_64 => &["amd64", "x86_64"],
            Arch::Aarch64 => &["arm64", "aarch64"],
            Arch::Other(_) => &[],
        }
    }
}

/// Architecture values that mean a package runs on any machine.
pub const NEUTRAL_ARCHES: [&str; 3] = ["all", "noarch", "any"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    Elf,
    Script,
}

impl ExecKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecKind::Elf => "elf",
            ExecKind::Script => "script",
        }
    }
}

impl FromStr for ExecKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "elf" => Ok(ExecKind::Elf),
            "script" => Ok(ExecKind::Script),
            other => Err(Error::Parse(format!("unknown exec kind: {other}"))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: PathBuf,
    pub file_name: String,
}

/// An executable found in a payload; `path` is relative to the payload root.
#[derive(Debug, Clone)]
pub struct EntryCandidate {
    pub path: PathBuf,
    pub kind: ExecKind,
    pub score: i32,
}

#[derive(Debug, Clone)]
pub struct Icon {
    /// One of "png", "svg", "xpm".
    pub ext: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Payload {
    pub format: FormatKind,
    pub name: String,
    pub version: Option<String>,
    pub strip_prefix: Option<PathBuf>,
    /// Sorted best first; empty for AppImage.
    pub entries: Vec<EntryCandidate>,
    pub desktop: Option<String>,
    pub icon: Option<Icon>,
    pub warnings: Vec<String>,
    /// Set only for deb, rpm and pacman payloads.
    pub package: Option<PackageMeta>,
}

/// A maintainer or install script, read as text and never run.
#[derive(Debug, Clone)]
pub struct Script {
    pub name: String,
    pub text: String,
}

/// What a package's payload contains. Keys are relative and normalized (no
/// leading `/` or `./`); link targets are stored as written.
#[derive(Debug, Clone, Default)]
pub struct PayloadIndex {
    pub files: BTreeSet<PathBuf>,
    pub dirs: BTreeSet<PathBuf>,
    pub links: BTreeMap<PathBuf, PathBuf>,
    pub setuid: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PackageMeta {
    pub family: PackageFamily,
    pub name: String,
    pub version: String,
    pub arch: String,
    pub depends: Vec<String>,
    pub scripts: Vec<Script>,
    pub index: PayloadIndex,
    pub elf: BTreeMap<PathBuf, ElfFacts>,
    /// Text of the chosen entry when it is a `#!` file of at most 256 KiB.
    pub entry_script: Option<String>,
    pub data: DataSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    LocalFile,
    DirectUrl,
    Github,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::LocalFile => "local-file",
            SourceKind::DirectUrl => "direct-url",
            SourceKind::Github => "github",
        }
    }
}

impl FromStr for SourceKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "local-file" => Ok(SourceKind::LocalFile),
            "direct-url" => Ok(SourceKind::DirectUrl),
            "github" => Ok(SourceKind::Github),
            other => Err(Error::Parse(format!("unknown source kind: {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCfg {
    pub kind: SourceKind,
    pub location: String,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub version: Option<String>,
    pub marker: Option<String>,
    pub assets: Vec<Asset>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_str() {
        for k in [
            FormatKind::AppImage,
            FormatKind::Tarball,
            FormatKind::Deb,
            FormatKind::Rpm,
            FormatKind::Pacman,
        ] {
            assert_eq!(k.as_str().parse::<FormatKind>().unwrap(), k);
        }
        for k in [ExecKind::Elf, ExecKind::Script] {
            assert_eq!(k.as_str().parse::<ExecKind>().unwrap(), k);
        }
        for k in [
            SourceKind::LocalFile,
            SourceKind::DirectUrl,
            SourceKind::Github,
        ] {
            assert_eq!(k.as_str().parse::<SourceKind>().unwrap(), k);
        }
        assert!("zip".parse::<FormatKind>().is_err());
    }

    #[test]
    fn arch_aliases() {
        assert_eq!(Arch::X86_64.aliases(), &["x86_64", "amd64", "x64"]);
        assert_eq!(Arch::Aarch64.aliases(), &["aarch64", "arm64"]);
        assert!(Arch::Other("riscv64".into()).aliases().is_empty());
    }

    #[test]
    fn package_families() {
        assert_eq!(FormatKind::Deb.family(), Some(PackageFamily::Deb));
        assert_eq!(FormatKind::Rpm.family(), Some(PackageFamily::Rpm));
        assert_eq!(FormatKind::Pacman.family(), Some(PackageFamily::Pacman));
        assert_eq!(FormatKind::Tarball.family(), None);
        assert_eq!(FormatKind::AppImage.family(), None);
    }

    #[test]
    fn arch_package_aliases() {
        assert_eq!(Arch::X86_64.package_aliases(), &["amd64", "x86_64"]);
        assert_eq!(Arch::Aarch64.package_aliases(), &["arm64", "aarch64"]);
        assert!(Arch::Other("riscv64".into()).package_aliases().is_empty());
        assert_eq!(NEUTRAL_ARCHES, ["all", "noarch", "any"]);
    }
}
