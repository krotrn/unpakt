use std::path::Path;

use crate::model::{Arch, PackageFamily};

const FUSE2_LIB_DIRS: [&str; 4] = [
    "/usr/lib",
    "/usr/lib64",
    "/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
];

/// What the installer needs to know about the machine it runs on.
#[derive(Debug, Clone)]
pub struct Host {
    pub arch: Arch,
    pub os_id: Option<String>,
    pub os_like: Vec<String>,
    pub immutable_root: bool,
    pub fuse2: bool,
}

impl Host {
    /// Probes the running system. Unreadable sources count as absent, so this never fails.
    pub fn detect() -> Host {
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let (os_id, os_like) = parse_os_release(&os_release);
        let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
        let immutable_root = root_is_readonly(&mounts) || Path::new("/run/ostree-booted").exists();
        let fuse2 = FUSE2_LIB_DIRS
            .iter()
            .any(|dir| Path::new(dir).join("libfuse.so.2").exists());
        Host {
            arch: Arch::current(),
            os_id,
            os_like,
            immutable_root,
            fuse2,
        }
    }
}

impl Host {
    /// The package family of this distribution, from `ID` then each `ID_LIKE` entry.
    pub fn family(&self) -> Option<PackageFamily> {
        self.os_id
            .iter()
            .chain(self.os_like.iter())
            .find_map(|id| family_of(id))
    }
}

fn family_of(id: &str) -> Option<PackageFamily> {
    match id {
        "debian" | "ubuntu" => Some(PackageFamily::Deb),
        "fedora" | "rhel" | "centos" | "suse" => Some(PackageFamily::Rpm),
        "arch" => Some(PackageFamily::Pacman),
        _ if id.starts_with("opensuse") => Some(PackageFamily::Rpm),
        _ => None,
    }
}

/// The command that installs `file` system-wide with the host's package manager.
pub fn native_install_hint(family: PackageFamily, file: &Path) -> String {
    let path = file.to_string_lossy();
    let safe = path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._+-".contains(c));
    let path = if safe {
        path.into_owned()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    };
    match family {
        PackageFamily::Deb => format!("sudo apt install {path}"),
        PackageFamily::Rpm => format!("sudo dnf install {path}"),
        PackageFamily::Pacman => format!("sudo pacman -U {path}"),
    }
}

/// Reads `ID` and `ID_LIKE` from os-release text.
pub fn parse_os_release(text: &str) -> (Option<String>, Vec<String>) {
    let mut id = None;
    let mut like = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "ID" => id = Some(value.to_string()),
            "ID_LIKE" => like = value.split_whitespace().map(str::to_string).collect(),
            _ => {}
        }
    }
    (id, like)
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// True when `/` or `/usr` is mounted with `ro` among its comma-separated options.
pub fn root_is_readonly(proc_mounts: &str) -> bool {
    proc_mounts.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [_, mount_point, _, options, ..] => {
                matches!(*mount_point, "/" | "/usr") && options.split(',').any(|opt| opt == "ro")
            }
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_with(id: &str, like: &[&str]) -> Host {
        Host {
            arch: Arch::current(),
            os_id: Some(id.to_string()),
            os_like: like.iter().map(|s| s.to_string()).collect(),
            immutable_root: false,
            fuse2: false,
        }
    }

    #[test]
    fn family_from_os_release() {
        assert_eq!(host_with("ubuntu", &[]).family(), Some(PackageFamily::Deb));
        assert_eq!(
            host_with("linuxmint", &["ubuntu", "debian"]).family(),
            Some(PackageFamily::Deb)
        );
        assert_eq!(
            host_with("opensuse-tumbleweed", &["suse"]).family(),
            Some(PackageFamily::Rpm)
        );
        assert_eq!(
            host_with("endeavouros", &["arch"]).family(),
            Some(PackageFamily::Pacman)
        );
        assert_eq!(host_with("nixos", &[]).family(), None);
    }

    #[test]
    fn install_hint_per_family() {
        assert_eq!(
            native_install_hint(PackageFamily::Pacman, Path::new("/tmp/a b.pkg.tar.zst")),
            "sudo pacman -U '/tmp/a b.pkg.tar.zst'"
        );
        assert_eq!(
            native_install_hint(PackageFamily::Deb, Path::new("/tmp/x.deb")),
            "sudo apt install /tmp/x.deb"
        );
    }

    #[test]
    fn parses_unquoted_id() {
        assert_eq!(
            parse_os_release("ID=arch\n"),
            (Some("arch".to_string()), vec![])
        );
    }

    #[test]
    fn parses_quoted_id_and_like() {
        assert_eq!(
            parse_os_release("ID=\"ubuntu\"\nID_LIKE=\"debian\"\n"),
            (Some("ubuntu".to_string()), vec!["debian".to_string()])
        );
    }

    #[test]
    fn skips_comments_and_blank_lines_and_strips_single_quotes() {
        let text = "# generated file\n\nNAME='Fedora Linux'\nID='fedora'\nID_LIKE='rhel centos'\n";
        assert_eq!(
            parse_os_release(text),
            (
                Some("fedora".to_string()),
                vec!["rhel".to_string(), "centos".to_string()]
            )
        );
    }

    #[test]
    fn rw_root_is_not_readonly() {
        assert!(!root_is_readonly("/dev/sda / ext4 rw 0 0\n"));
    }

    #[test]
    fn ro_usr_mount_is_readonly() {
        assert!(root_is_readonly("composefs /usr overlay ro 0 0"));
    }

    #[test]
    fn other_root_options_are_not_readonly() {
        assert!(!root_is_readonly(
            "/dev/sda / ext4 rw,errors=remount-ro 0 0\n"
        ));
        assert!(!root_is_readonly("/dev/sda / ext4 rw,noatime 0 0\n"));
    }
}
