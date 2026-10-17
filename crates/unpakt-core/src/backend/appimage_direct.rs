use super::{Backend, Installed, Plan, version_dir};
use crate::error::{IoResultExt, Result};
use crate::model::{ExecKind, FormatKind, Payload};
use crate::{Layout, Txn};

pub struct AppImageDirect;

impl Backend for AppImageDirect {
    fn id(&self) -> &'static str {
        "appimage-direct"
    }

    fn applicable(&self, p: &Payload) -> bool {
        p.format == FormatKind::AppImage
    }

    fn install(&self, plan: &Plan, layout: &Layout, txn: &mut Txn) -> Result<Installed> {
        let dir = version_dir(plan, layout)?;
        let file_name = format!("{}.AppImage", plan.id);
        let bytes = std::fs::read(&plan.artifact.path).at(&plan.artifact.path)?;
        txn.create_dir_all(&dir)?;
        txn.write(&dir.join(&file_name), &bytes, 0o755)?;
        Ok(Installed {
            version_dir: dir,
            entry: file_name.into(),
            kind: ExecKind::Elf,
        })
    }
}
