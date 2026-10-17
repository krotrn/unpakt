//! Ties inspection, backends, integration, the manifest and the undo journal
//! together. Installs and removals are all-or-nothing.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::backend::{Plan, for_payload};
use crate::error::{Error, IoResultExt, Result};
use crate::format::detect;
use crate::host::Host;
use crate::integrate::{IntegrateReq, disintegrate, integrate};
use crate::manifest::{AppRecord, FileRecord, Manifest, VersionRecord};
use crate::model::{Artifact, FormatKind, SourceCfg, SourceKind};
use crate::naming::{slug, version_label};
use crate::{Layout, Txn};

const UPDATE_DESKTOP_DATABASE: &str = "update-desktop-database";

#[derive(Debug, Default, Clone)]
pub struct AddOpts {
    pub id: Option<String>,
    pub entry: Option<PathBuf>,
    pub command: Option<String>,
    pub desktop: Option<bool>,
    pub no_bin: bool,
    pub force: bool,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub plan: Plan,
    pub source: SourceCfg,
    pub marker: Option<String>,
    pub backend: &'static str,
    pub want_desktop: bool,
    pub command: Option<String>,
    pub opts: AddOpts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub struct Engine {
    layout: Layout,
    manifest: Manifest,
    host: Host,
    path_var: OsString,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sha256_prefix(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).at(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).at(path)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize())[..12].to_string())
}

/// Finds a system helper tool. Directories unpakt itself writes into (the bin
/// dir) and anything resolving into the data dir are never trusted, so an
/// installed package cannot masquerade as a helper.
fn find_helper(program: &str, path_var: &std::ffi::OsStr, layout: &Layout) -> Option<PathBuf> {
    let data = layout.data.canonicalize().unwrap_or(layout.data.clone());
    std::env::split_paths(path_var)
        .filter(|d| !d.as_os_str().is_empty() && !same_dir(d, &layout.bin))
        .map(|d| d.join(program))
        .filter(|p| p.is_file())
        .find(|p| p.canonicalize().is_ok_and(|c| !c.starts_with(&data)))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

impl Engine {
    pub fn new(layout: Layout, host: Host) -> Result<Engine> {
        let manifest = Manifest::open(&layout.db)?;
        let mut txn = Txn::new();
        for dir in [
            &layout.apps,
            &layout.applications,
            &layout.icons,
            &layout.bin,
        ] {
            txn.create_dir_all(dir)?;
        }
        txn.commit();
        Ok(Engine {
            layout,
            manifest,
            host,
            path_var: std::env::var_os("PATH").unwrap_or_default(),
        })
    }

    pub fn with_path_var(mut self, path_var: OsString) -> Engine {
        self.path_var = path_var;
        self
    }

    pub fn open_default() -> Result<Engine> {
        Engine::new(Layout::user()?, Host::detect())
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn inspect(&self, input: &str, opts: AddOpts) -> Result<InstallPlan> {
        let path = Path::new(input);
        if !path.is_file() {
            return Err(Error::NotFound(format!("{input} is not an existing file")));
        }
        let canonical = path.canonicalize().at(path)?;
        let file_name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let artifact = Artifact {
            path: canonical.clone(),
            file_name,
        };
        let payload = detect(&artifact)?.inspect(&artifact)?;
        let backend = for_payload(&payload).ok_or_else(|| {
            Error::Unsupported(format!(
                "no backend can install a {}",
                payload.format.as_str()
            ))
        })?;

        let id = match &opts.id {
            Some(id) => {
                if id.is_empty() || slug(id) != *id {
                    return Err(Error::Refused(format!(
                        "invalid app id `{id}`: use lowercase letters, digits and `-`"
                    )));
                }
                id.clone()
            }
            None => {
                let id = slug(&payload.name);
                if id.is_empty() {
                    return Err(Error::Refused("cannot derive an app id; pass --id".into()));
                }
                id
            }
        };

        let label = match &payload.version {
            Some(v) => version_label(v),
            None => sha256_prefix(&canonical)?,
        };

        let entry = match &opts.entry {
            Some(wanted) => Some(
                payload
                    .entries
                    .iter()
                    .find(|e| e.path == *wanted)
                    .cloned()
                    .ok_or_else(|| {
                        Error::NotFound(format!("no entry {} in the package", wanted.display()))
                    })?,
            ),
            None => payload.entries.first().cloned(),
        };

        let want_desktop = opts.desktop.unwrap_or(
            payload.format == FormatKind::AppImage
                || payload.desktop.is_some()
                || payload.icon.is_some(),
        );
        let command = if opts.no_bin {
            None
        } else if let Some(c) = &opts.command {
            Some(c.clone())
        } else {
            match (&entry, payload.format) {
                (Some(e), FormatKind::Tarball) => {
                    e.path.file_stem().map(|s| s.to_string_lossy().into_owned())
                }
                _ => Some(id.clone()),
            }
        };

        let source = SourceCfg {
            kind: SourceKind::LocalFile,
            location: canonical.to_string_lossy().into_owned(),
            pattern: None,
        };
        let plan = Plan {
            id,
            name: payload.name.clone(),
            artifact,
            payload,
            label,
            entry,
        };
        Ok(InstallPlan {
            plan,
            source,
            marker: None,
            backend: backend.id(),
            want_desktop,
            command,
            opts,
        })
    }

    pub fn install(&mut self, p: InstallPlan) -> Result<AppRecord> {
        let id = p.plan.id.clone();
        if self.manifest.get(&id)?.is_some() {
            return Err(Error::Conflict(format!(
                "{id} is already installed; use `unpakt update {id}`"
            )));
        }
        let backend = for_payload(&p.plan.payload)
            .ok_or_else(|| Error::Unsupported("no backend can install this package".into()))?;

        let mut txn = Txn::new();
        let installed = backend.install(&p.plan, &self.layout, &mut txn)?;
        let current = self.layout.apps.join(&id).join("current");
        txn.symlink_swap(Path::new(&p.plan.label), &current)?;

        let exec = current.join(&installed.entry);
        let integration = integrate(
            &self.layout,
            &IntegrateReq {
                id: &id,
                name: &p.plan.name,
                exec: &exec,
                kind: installed.kind,
                desktop_src: p.plan.payload.desktop.as_deref(),
                icon: p.plan.payload.icon.as_ref(),
                want_desktop: p.want_desktop,
                command: p.command.as_deref(),
                force: p.opts.force,
                no_fuse_appimage: p.plan.payload.format == FormatKind::AppImage && !self.host.fuse2,
                owned: &[],
                path_var: &self.path_var,
            },
            &mut txn,
        )?;
        for warning in integration.warnings.iter().chain(&p.plan.payload.warnings) {
            tracing::warn!("{id}: {warning}");
        }

        let app = AppRecord {
            id: id.clone(),
            name: p.plan.name.clone(),
            format: p.plan.payload.format,
            backend: p.backend.to_string(),
            source: p.source.clone(),
            current: p.plan.label.clone(),
            desktop: p.want_desktop,
            command: p.command.clone(),
        };
        let version = VersionRecord {
            label: p.plan.label.clone(),
            version: p.plan.payload.version.clone(),
            marker: p.marker.clone(),
            entry: installed.entry,
            exec_kind: installed.kind,
            installed_at: now(),
        };
        let entry_opt = p
            .opts
            .entry
            .as_ref()
            .map(|e| e.to_string_lossy().into_owned());
        let mut options: Vec<(&str, &str)> = Vec::new();
        if let Some(entry) = &entry_opt {
            options.push(("entry", entry));
        }
        if let Some(pattern) = &p.opts.pattern {
            options.push(("pattern", pattern));
        }
        self.manifest
            .commit_install_with(&app, &version, &integration.files, &options)?;
        txn.commit();
        self.refresh_desktop_database();
        Ok(app)
    }

    pub fn list(&self) -> Result<Vec<AppRecord>> {
        self.manifest.list()
    }

    pub fn get(&self, id: &str) -> Result<(AppRecord, Vec<VersionRecord>, Vec<FileRecord>)> {
        let app = self
            .manifest
            .get(id)?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        let versions = self.manifest.versions(id)?;
        let files = self.manifest.files(id)?;
        Ok((app, versions, files))
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        let (_, _, files) = self.get(id)?;
        let mut txn = Txn::new();
        disintegrate(&files, &mut txn)?;
        let dir = self.layout.apps.join(id);
        if std::fs::symlink_metadata(&dir).is_ok() {
            txn.remove_dir_all(&dir)?;
        }
        self.manifest.remove(id)?;
        txn.commit();
        self.refresh_desktop_database();
        Ok(())
    }

    pub fn doctor(&self) -> Vec<Check> {
        let mut checks = Vec::new();

        let bin_in_path = std::env::split_paths(&self.path_var)
            .any(|d| !d.as_os_str().is_empty() && same_dir(&d, &self.layout.bin));
        checks.push(Check {
            name: "bin-in-path".into(),
            ok: bin_in_path,
            detail: if bin_in_path {
                format!("{} is in PATH", self.layout.bin.display())
            } else {
                format!("{} is not in PATH", self.layout.bin.display())
            },
        });

        checks.push(Check {
            name: "fuse2".into(),
            ok: self.host.fuse2,
            detail: if self.host.fuse2 {
                "libfuse.so.2 found".into()
            } else {
                "libfuse.so.2 not found; AppImages run with APPIMAGE_EXTRACT_AND_RUN=1".into()
            },
        });

        let udd = find_helper(UPDATE_DESKTOP_DATABASE, &self.path_var, &self.layout);
        checks.push(Check {
            name: UPDATE_DESKTOP_DATABASE.into(),
            ok: udd.is_some(),
            detail: match &udd {
                Some(p) => format!("found at {}", p.display()),
                None => "not found in PATH; launcher entries may appear late".into(),
            },
        });

        match self.manifest.list() {
            Ok(apps) => {
                for app in apps {
                    checks.push(self.check_app(&app));
                }
            }
            Err(e) => checks.push(Check {
                name: "manifest".into(),
                ok: false,
                detail: e.to_string(),
            }),
        }
        checks
    }

    fn check_app(&self, app: &AppRecord) -> Check {
        let name = format!("app:{}", app.id);
        let current = self.layout.apps.join(&app.id).join("current");
        if std::fs::metadata(&current).is_err() {
            return Check {
                name,
                ok: false,
                detail: format!("{} does not resolve", current.display()),
            };
        }
        match self.manifest.files(&app.id) {
            Ok(files) => {
                let missing: Vec<String> = files
                    .iter()
                    .filter(|f| std::fs::symlink_metadata(&f.path).is_err())
                    .map(|f| f.path.display().to_string())
                    .collect();
                if missing.is_empty() {
                    Check {
                        name,
                        ok: true,
                        detail: format!("version {} is intact", app.current),
                    }
                } else {
                    Check {
                        name,
                        ok: false,
                        detail: format!("missing: {}", missing.join(", ")),
                    }
                }
            }
            Err(e) => Check {
                name,
                ok: false,
                detail: e.to_string(),
            },
        }
    }

    /// Asks the desktop to re-read launcher entries. Never runs anything from
    /// an installed package; failures are only logged.
    fn refresh_desktop_database(&self) {
        let Some(tool) = find_helper(UPDATE_DESKTOP_DATABASE, &self.path_var, &self.layout) else {
            return;
        };
        match Command::new(&tool)
            .arg(&self.layout.applications)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!("{UPDATE_DESKTOP_DATABASE} exited with {status}"),
            Err(e) => tracing::warn!("could not run {UPDATE_DESKTOP_DATABASE}: {e}"),
        }
    }
}
