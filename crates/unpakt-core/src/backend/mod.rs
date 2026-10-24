//! Backends turn an inspected package into an installed version directory.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::cancel::Cancel;
use crate::error::{Error, IoResultExt, Result};
use crate::model::{Artifact, EntryCandidate, ExecKind, Payload};
use crate::{Layout, Txn};

mod appimage_direct;
mod extract;

/// Everything a backend needs to install one version of one app.
#[derive(Debug, Clone)]
pub struct Plan {
    pub id: String,
    pub name: String,
    pub artifact: Artifact,
    pub payload: Payload,
    pub label: String,
    /// `None` for AppImages, which are their own entry.
    pub entry: Option<EntryCandidate>,
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub version_dir: PathBuf,
    /// Relative to `version_dir`.
    pub entry: PathBuf,
    pub kind: ExecKind,
}

pub trait Backend: Sync {
    fn id(&self) -> &'static str;
    fn applicable(&self, p: &Payload) -> bool;
    fn install(&self, plan: &Plan, layout: &Layout, txn: &mut Txn) -> Result<Installed>;
}

static APPIMAGE_DIRECT: appimage_direct::AppImageDirect = appimage_direct::AppImageDirect;
static EXTRACT: extract::Extract = extract::Extract;

/// The first backend that can install `p`.
pub fn for_payload(p: &Payload) -> Option<&'static dyn Backend> {
    let all: [&'static dyn Backend; 2] = [&APPIMAGE_DIRECT, &EXTRACT];
    all.into_iter().find(|b| b.applicable(p))
}

/// `apps/<id>/<label>`, refusing one that already exists.
fn version_dir(plan: &Plan, layout: &Layout) -> Result<PathBuf> {
    let dir = layout.apps.join(&plan.id).join(&plan.label);
    if std::fs::symlink_metadata(&dir).is_ok() {
        return Err(Error::Conflict(format!(
            "{} {} is already installed",
            plan.id, plan.label
        )));
    }
    Ok(dir)
}

const COPY_CHUNK: usize = 64 * 1024;

/// Copies `from` into `to`, checking `cancel` between chunks.
fn copy_cancellable(
    from: &mut dyn Read,
    to: &mut dyn Write,
    cancel: &Cancel,
    at: &Path,
) -> Result<()> {
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        cancel.check()?;
        let n = from.read(&mut buf).at(at)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n]).at(at)?;
    }
}
