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

/// Whether the program of a system-wide app is still where it was installed.
pub(super) fn native_check(entry: &Path) -> (bool, String) {
    if entry.exists() {
        (true, "installed system-wide by the package manager".into())
    } else {
        (
            false,
            format!(
                "{} is missing; the package may have been removed outside unpakt",
                entry.display()
            ),
        )
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
        self.apply_native(NativeOp::Install, &id, &artifact, &consent, record, &[])
    }

    /// Journals the change, has PackageKit install `artifact`, then records
    /// what landed. The journal stays unless nothing was changed, so a later
    /// run can settle an answer that was lost.
    fn apply_native(
        &mut self,
        op: NativeOp,
        id: &str,
        artifact: &Path,
        consent: &NativeConsent,
        record: NativeRecord,
        pruned: &[&str],
    ) -> Result<AppRecord> {
        let journal = NativeJournal::create(
            &self.layout.data.join("native"),
            &NativeIntent {
                op,
                app: id.to_string(),
                pk_name: consent.name.clone(),
                version: consent.version.clone(),
                record: Some(record.clone()),
            },
        )?;

        self.emit(Progress::Installing);
        let result = self
            .packagekit
            .install_file(artifact, &self.cancel, &mut |event| {
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
        let app = record.app_record(id)?;
        let version = record.version_record();
        let mut stored: Vec<(&str, &str)> = record
            .options
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        stored.push(("pk.package_id", &installed.id));
        self.manifest
            .commit_install_with(&app, &version, &[], &stored, pruned)?;
        journal.finish()?;
        Ok(app)
    }

    /// Updates a system-wide app to the latest release, after the person
    /// agrees. The package manager is the only route; a system package that is
    /// already as new is recorded instead of reinstalled.
    pub(super) fn update_native(
        &mut self,
        app: &AppRecord,
        current: &VersionRecord,
        opts: UpdateOpts<'_>,
    ) -> Result<Option<String>> {
        let id = app.id.as_str();
        let release = for_kind(app.source.kind).latest(&app.source, &self.http)?;
        if app.source.kind == SourceKind::Github && newer_release(app, current, &release).is_none()
        {
            return Ok(None);
        }
        let pattern = self
            .manifest
            .option(id, "pattern")?
            .or_else(|| app.source.pattern.clone());
        let add = AddOpts {
            id: Some(id.to_string()),
            format: Some(app.format),
            pattern,
            extract: false,
            ..Default::default()
        };
        let p = self.inspect(&app.source.location, add)?;
        if p.backend != NATIVE_BACKEND {
            self.discard(p);
            return Err(Error::PackageKitUnavailable(format!(
                "{id} was installed with PackageKit, which is not available now; current version kept"
            )));
        }
        let _download = self.download_guard(&p.plan.artifact.path);
        let label = p.plan.label.clone();
        if label == current.label {
            return Ok(None);
        }
        // The release that was just fetched may differ from the one checked
        // above, so the downgrade guard applies to what is about to be installed.
        if app.source.kind == SourceKind::Github {
            let fetched = Release {
                version: p.marker.clone(),
                marker: p.marker.clone(),
                assets: Vec::new(),
            };
            if newer_release(app, current, &fetched).is_none() {
                return Ok(None);
            }
        }
        let consent = p
            .consent()
            .ok_or_else(|| Error::Unsupported("this package has no native install".into()))?;
        let pk_name = self
            .manifest
            .option(id, "pk.name")?
            .unwrap_or_else(|| consent.name.clone());
        let installed = self.packagekit.resolve_installed(&pk_name)?;
        let Some(installed) = installed else {
            return Err(Error::NotFound(format!(
                "{id} is no longer installed on the system; run `unpakt remove {id}` to forget it"
            )));
        };
        if crate::version::compare_package(&consent.version, &installed.version)
            != std::cmp::Ordering::Greater
        {
            let system_label = version_label(&installed.version);
            if system_label == current.label {
                return Ok(None);
            }
            let _locked = self.lock_unchanged(app)?;
            let record = AppRecord {
                current: system_label.clone(),
                ..app.clone()
            };
            let version = VersionRecord {
                label: system_label,
                version: Some(installed.version.clone()),
                marker: p.marker.clone(),
                entry: current.entry.clone(),
                exec_kind: current.exec_kind,
                installed_at: now(),
            };
            self.manifest.commit_install_with(
                &record,
                &version,
                &[],
                &[("pk.package_id", &installed.id)],
                &[],
            )?;
            return Ok(None);
        }

        match opts.confirm_native {
            None => return Err(Error::NeedsConsent(consent.message())),
            Some(confirm) => {
                if !confirm(&consent) {
                    return Err(Error::Refused(format!(
                        "update of {id} declined; current version kept"
                    )));
                }
            }
        }

        let artifact = std::fs::canonicalize(&p.plan.artifact.path).at(&p.plan.artifact.path)?;
        let locked = self.lock_unchanged(app)?;
        // `versions` is newest first; the new version takes the first slot.
        let pruned: Vec<&str> = locked
            .versions
            .iter()
            .skip(KEEP_VERSIONS - 1)
            .filter(|v| v.label != label)
            .map(|v| v.label.as_str())
            .collect();
        let record = NativeRecord {
            name: p.plan.name.clone(),
            format: app.format.as_str().into(),
            source_kind: app.source.kind.as_str().into(),
            source_location: app.source.location.clone(),
            source_pattern: app.source.pattern.clone(),
            label: label.clone(),
            version: p.plan.payload.version.clone(),
            marker: p.marker.clone(),
            entry: native_entry(&p.plan.payload, p.plan.entry.as_ref()),
            options: Vec::new(),
        };
        self.apply_native(NativeOp::Update, id, &artifact, &consent, record, &pruned)?;
        Ok(Some(label))
    }

    pub(super) fn remove_native(&mut self, app: &AppRecord) -> Result<()> {
        let id = &app.id;
        if !self.packagekit_ready() {
            return Err(Error::PackageKitUnavailable(format!(
                "{id} was installed with PackageKit, which is not available now; run `unpakt remove --forget {id}` to drop the record and leave the package installed"
            )));
        }
        let Some(pk_name) = self.manifest.option(id, "pk.name")? else {
            return Err(Error::PackageKit(format!(
                "{id} has no recorded package name; run `unpakt remove --forget {id}`"
            )));
        };
        let journal = NativeJournal::create(
            &self.layout.data.join("native"),
            &NativeIntent {
                op: NativeOp::Remove,
                app: id.clone(),
                pk_name: pk_name.clone(),
                version: String::new(),
                record: None,
            },
        )?;

        // A refused or failed removal leaves a stale record, which is safe;
        // dropping the journal avoids a later run untracking a package that
        // is still installed.
        match self.packagekit.resolve_installed(&pk_name) {
            Err(e) => {
                if let Err(settle) = journal.finish() {
                    tracing::warn!("{id}: could not remove the native journal: {settle}");
                }
                return Err(e);
            }
            Ok(Some(package)) => {
                self.emit(Progress::Installing);
                let result = self
                    .packagekit
                    .remove(&package.id, &self.cancel, &mut |event| {
                        self.emit(match event {
                            PkEvent::WaitingForAuth => Progress::WaitingForAuthorization,
                            PkEvent::Changing { percent } => Progress::SystemChange { percent },
                        })
                    });
                if let Err(e) = result {
                    if let Err(settle) = journal.finish() {
                        tracing::warn!("{id}: could not remove the native journal: {settle}");
                    }
                    return Err(e);
                }
            }
            Ok(None) => {}
        }
        self.manifest.remove(id)?;
        journal.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_check_cases() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            native_check(file.path()),
            (true, "installed system-wide by the package manager".into())
        );
        let (ok, detail) = native_check(Path::new("/nonexistent/unpakt/bin"));
        assert!(!ok);
        assert!(detail.contains("removed outside unpakt"));
    }
}
