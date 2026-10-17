use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Every on-disk location unpakt uses, rooted at a home-like directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub data: PathBuf,
    pub apps: PathBuf,
    pub db: PathBuf,
    pub applications: PathBuf,
    pub icons: PathBuf,
    pub bin: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
}

impl Layout {
    /// The layout beneath `root`, which stands in for the home directory.
    pub fn under(root: &Path) -> Layout {
        let data = root.join(".local/share/unpakt");
        Layout {
            apps: data.join("apps"),
            db: data.join("db.sqlite"),
            data,
            applications: root.join(".local/share/applications"),
            icons: root.join(".local/share/icons/hicolor"),
            bin: root.join(".local/bin"),
            config: root.join(".config/unpakt"),
            cache: root.join(".cache/unpakt"),
        }
    }

    /// The layout for the current user: `UNPAKT_ROOT` if set, else the home directory.
    pub fn user() -> Result<Layout> {
        match std::env::var_os("UNPAKT_ROOT").filter(|v| !v.is_empty()) {
            Some(root) => Ok(Layout::under(Path::new(&root))),
            None => {
                let home =
                    dirs::home_dir().ok_or_else(|| Error::NotFound("home directory".into()))?;
                Ok(Layout::under(&home))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_maps_every_location() {
        let l = Layout::under(Path::new("/r"));
        assert_eq!(l.data, Path::new("/r/.local/share/unpakt"));
        assert_eq!(l.apps, Path::new("/r/.local/share/unpakt/apps"));
        assert_eq!(l.db, Path::new("/r/.local/share/unpakt/db.sqlite"));
        assert_eq!(l.applications, Path::new("/r/.local/share/applications"));
        assert_eq!(l.icons, Path::new("/r/.local/share/icons/hicolor"));
        assert_eq!(l.bin, Path::new("/r/.local/bin"));
        assert_eq!(l.config, Path::new("/r/.config/unpakt"));
        assert_eq!(l.cache, Path::new("/r/.cache/unpakt"));
    }
}
