//! Installing a package through the system's package manager.

use super::*;
use crate::packagekit::PkEvent;
use crate::packagekit::journal::{NativeIntent, NativeJournal, NativeOp, NativeRecord};

/// The path the package's program is run from once installed: the
/// `usr/bin`/`bin` link that leads to the entry, else the entry itself, else
/// `/usr/bin/<package name>`.
pub(super) fn native_entry(payload: &Payload, entry: Option<&EntryCandidate>) -> PathBuf {
    let root = Path::new("/");
    let meta = payload.package.as_ref();
    match (meta, entry) {
        (Some(meta), Some(entry)) => match bin_link(&meta.index, &entry.path) {
            Some(link) => root.join(link),
            None => root.join(&entry.path),
        },
        _ => root
            .join("usr/bin")
            .join(meta.map_or(&payload.name, |m| &m.name)),
    }
}

impl Engine {
    pub(super) fn install_native(&mut self, p: InstallPlan) -> Result<AppRecord> {
        let consent = p
            .consent()
            .ok_or_else(|| Error::Unsupported("this package has no native install".into()))?;
        if !p.native_consent {
            return Err(Error::NeedsConsent(consent.message()));
        }
        let _download = self.download_guard(&p.plan.artifact.path);
        let id = p.plan.id.clone();
        let _lock = self.begin()?;
        if self.manifest.get(&id)?.is_some() {
            return Err(Error::Conflict(format!(
                "{id} is already installed; use `unpakt update {id}`"
            )));
        }
        let artifact = std::fs::canonicalize(&p.plan.artifact.path).at(&p.plan.artifact.path)?;

        let mut options: Vec<(String, String)> = vec![("pk.name".into(), consent.name.clone())];
        if let Some(pattern) = &p.opts.pattern {
            options.push(("pattern".into(), pattern.clone()));
        }
        if let Some(format) = p.opts.format {
            options.push(("format".into(), format.as_str().into()));
        }
        let record = NativeRecord {
            name: p.plan.name.clone(),
            format: p.plan.payload.format.as_str().into(),
            source_kind: p.source.kind.as_str().into(),
            source_location: p.source.location.clone(),
            source_pattern: p.source.pattern.clone(),
            label: p.plan.label.clone(),
            version: p.plan.payload.version.clone(),
            marker: p.marker.clone(),
            entry: native_entry(&p.plan.payload, p.plan.entry.as_ref()),
            options,
        };
        let journal = NativeJournal::create(
            &self.layout.data.join("native"),
            &NativeIntent {
                op: NativeOp::Install,
                app: id.clone(),
                pk_name: consent.name.clone(),
                version: consent.version.clone(),
                record: Some(record.clone()),
            },
        )?;

        self.emit(Progress::Installing);
        let result = self
            .packagekit
            .install_file(&artifact, &self.cancel, &mut |event| {
                self.emit(match event {
                    PkEvent::WaitingForAuth => Progress::WaitingForAuthorization,
                    PkEvent::Changing { percent } => Progress::SystemChange { percent },
                })
            });
        match result {
            Ok(()) => {}
            // Nothing was changed; any other failure may have been applied
            // after we stopped listening, so the journal stays for recovery.
            Err(e @ (Error::Refused(_) | Error::Interrupted)) => {
                if let Err(settle) = journal.finish() {
                    tracing::warn!("{id}: could not remove the native journal: {settle}");
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }

        let installed = self
            .packagekit
            .resolve_installed(&consent.name)?
            .ok_or_else(|| {
                Error::PackageKit(format!(
                    "{} was installed but PackageKit cannot find it",
                    consent.name
                ))
            })?;
        let app = record.app_record(&id)?;
        let version = record.version_record();
        let mut stored: Vec<(&str, &str)> = record
            .options
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        stored.push(("pk.package_id", &installed.id));
        self.manifest
            .commit_install_with(&app, &version, &[], &stored, &[])?;
        journal.finish()?;
        Ok(app)
    }
}
