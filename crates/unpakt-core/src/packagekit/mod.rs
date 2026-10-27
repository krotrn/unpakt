//! The system package manager, reached through PackageKit.

pub mod dbus;
pub(crate) mod journal;

use std::path::Path;

use crate::cancel::Cancel;
use crate::error::{Error, Result};

/// A package as PackageKit identifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkPackage {
    /// The whole PackageKit id, `name;version;arch;data`.
    pub id: String,
    pub name: String,
    pub version: String,
    pub arch: String,
}

impl PkPackage {
    /// Splits a PackageKit id; `None` when it is not `name;version;arch;data`
    /// with a name and a version.
    pub fn parse(id: &str) -> Option<PkPackage> {
        let mut fields = id.split(';');
        let (name, version, arch, _data) = (
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next()?,
        );
        if fields.next().is_some() || name.is_empty() || version.is_empty() {
            return None;
        }
        Some(PkPackage {
            id: id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            arch: arch.to_string(),
        })
    }
}

/// What a PackageKit transaction reports while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkEvent {
    /// Waiting for the user to authorize the change.
    WaitingForAuth,
    /// The package manager has started changing the system.
    Changing { percent: Option<u8> },
}

pub trait PackageKit: Send {
    /// PackageKit is reachable and its backend can install local files.
    fn can_install_files(&self) -> bool;
    /// The installed package called `name`, if any.
    fn resolve_installed(&self, name: &str) -> Result<Option<PkPackage>>;
    /// Installs the local package at the absolute path `file`.
    fn install_file(&self, file: &Path, cancel: &Cancel, on: &mut dyn FnMut(PkEvent))
    -> Result<()>;
    fn remove(&self, package_id: &str, cancel: &Cancel, on: &mut dyn FnMut(PkEvent)) -> Result<()>;
}

/// Stands in when PackageKit cannot be used.
pub struct Unavailable;

fn unavailable() -> Error {
    Error::PackageKitUnavailable("PackageKit is not available".into())
}

impl PackageKit for Unavailable {
    fn can_install_files(&self) -> bool {
        false
    }

    fn resolve_installed(&self, _name: &str) -> Result<Option<PkPackage>> {
        Err(unavailable())
    }

    fn install_file(&self, _: &Path, _: &Cancel, _: &mut dyn FnMut(PkEvent)) -> Result<()> {
        Err(unavailable())
    }

    fn remove(&self, _: &str, _: &Cancel, _: &mut dyn FnMut(PkEvent)) -> Result<()> {
        Err(unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_package_id() {
        let p = PkPackage::parse("foo;1.2-1;amd64;local").unwrap();
        assert_eq!(p.name, "foo");
        assert_eq!(p.version, "1.2-1");
        assert_eq!(p.arch, "amd64");
        assert_eq!(p.id, "foo;1.2-1;amd64;local");
    }

    #[test]
    fn rejects_malformed_ids() {
        for id in ["foo", "foo;1", ";1;a;b", "a;;b;c", "a;1;b;c;d"] {
            assert!(PkPackage::parse(id).is_none(), "{id}");
        }
    }

    #[test]
    fn unavailable_does_nothing() {
        assert!(!Unavailable.can_install_files());
        assert!(matches!(
            Unavailable.resolve_installed("x"),
            Err(Error::PackageKitUnavailable(_))
        ));
    }
}
