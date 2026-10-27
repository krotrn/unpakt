use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use unpakt_core::cancel::Cancel;
use unpakt_core::packagekit::{PackageKit, PkEvent, PkPackage};
use unpakt_core::{Error, Result};

#[derive(Default)]
pub struct FakeState {
    pub can_install: bool,
    pub installed: BTreeMap<String, PkPackage>,
    pub next_install: Option<PkPackage>,
    pub calls: Vec<String>,
    pub file_existed: Vec<bool>,
    pub events: Vec<PkEvent>,
    pub fail_install: Option<fn() -> Error>,
    pub fail_remove: Option<fn() -> Error>,
    pub fail_resolve_once: bool,
}

#[derive(Clone, Default)]
pub struct FakePk(pub Arc<Mutex<FakeState>>);

impl FakePk {
    /// Emits the scripted events, stopping with `Interrupted` when `cancel`
    /// is set by the time the first `Changing` (or the end) comes up.
    fn emit(events: &[PkEvent], cancel: &Cancel, on: &mut dyn FnMut(PkEvent)) -> Result<()> {
        for event in events {
            if matches!(event, PkEvent::Changing { .. }) {
                cancel.check()?;
            }
            on(*event);
        }
        cancel.check()
    }
}

impl PackageKit for FakePk {
    fn can_install_files(&self) -> bool {
        self.0.lock().unwrap().can_install
    }

    fn resolve_installed(&self, name: &str) -> Result<Option<PkPackage>> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(format!("resolve {name}"));
        if std::mem::take(&mut state.fail_resolve_once) {
            return Err(Error::PackageKit("resolve failed".into()));
        }
        Ok(state.installed.get(name).cloned())
    }

    fn install_file(
        &self,
        file: &Path,
        cancel: &Cancel,
        on: &mut dyn FnMut(PkEvent),
    ) -> Result<()> {
        let events = {
            let mut state = self.0.lock().unwrap();
            state.calls.push(format!("install {}", file.display()));
            state.file_existed.push(file.exists());
            state.events.clone()
        };
        Self::emit(&events, cancel, on)?;
        let mut state = self.0.lock().unwrap();
        if let Some(fail) = state.fail_install {
            return Err(fail());
        }
        if let Some(pkg) = state.next_install.take() {
            state.installed.insert(pkg.name.clone(), pkg);
        }
        Ok(())
    }

    fn remove(&self, package_id: &str, cancel: &Cancel, on: &mut dyn FnMut(PkEvent)) -> Result<()> {
        let events = {
            let mut state = self.0.lock().unwrap();
            state.calls.push(format!("remove {package_id}"));
            state.events.clone()
        };
        Self::emit(&events, cancel, on)?;
        let mut state = self.0.lock().unwrap();
        if let Some(fail) = state.fail_remove {
            return Err(fail());
        }
        state.installed.retain(|_, pkg| pkg.id != package_id);
        Ok(())
    }
}
