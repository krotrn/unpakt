//! Ties inspection, backends, integration, the manifest and the undo journal
//! together. Installs and removals are all-or-nothing.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use sha2::{Digest, Sha256};
use url::Url;

use crate::backend::{Plan, for_payload};
use crate::cancel::Cancel;
use crate::compat::{Verdict, check as check_compat};
use crate::desktop::{DesktopEntry, exec_argv0};
use crate::error::{Error, IoResultExt, Result};
use crate::format::detect;
use crate::format::package::resolve_in_payload;
use crate::host::{Host, native_download_hint, native_install_hint};
use crate::http::Http;
use crate::integrate::{IntegrateReq, disintegrate, integrate};
use crate::journal::{Expect, Intent};
use crate::lock::{self, WriteLock};
use crate::manifest::{AppRecord, FileKind, FileRecord, Manifest, VersionRecord};
use crate::model::{
    Artifact, EntryCandidate, FormatKind, Icon, Payload, PayloadIndex, Release, SourceCfg,
    SourceKind,
};
use crate::naming::{slug, validate_command, version_label};
use crate::progress::{Progress, ProgressFn};
use crate::recover::{Recovered, journals, recover};
use crate::select::choose;
use crate::source::{for_kind, resolve};
use crate::version::is_newer;
use crate::{Layout, Txn};

const UPDATE_DESKTOP_DATABASE: &str = "update-desktop-database";

/// Shortest gap between two download progress events.
const DOWNLOAD_EVENT_INTERVAL: Duration = Duration::from_millis(100);

/// How many versions of one app stay on disk.
pub const KEEP_VERSIONS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub id: String,
    pub current: String,
    pub latest: String,
}

/// The outcome of looking for updates.
#[derive(Debug, Default)]
pub struct UpdateCheck {
    pub updates: Vec<UpdateInfo>,
    /// Apps whose release could not be fetched, with the reason.
    pub failed: Vec<(String, Error)>,
}

/// The write lock plus the state of one app read while holding it.
struct Locked {
    lock: WriteLock,
    versions: Vec<VersionRecord>,
    files: Vec<FileRecord>,
}

/// Removes a per-operation download directory when dropped.
struct DownloadGuard(Option<PathBuf>);

impl DownloadGuard {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take()
            && let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("cannot remove download dir {}: {e}", dir.display());
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct AddOpts {
    pub id: Option<String>,
    pub entry: Option<PathBuf>,
    pub command: Option<String>,
    pub desktop: Option<bool>,
    pub no_bin: bool,
    pub force: bool,
    pub pattern: Option<String>,
    /// Restricts the release asset to this format.
    pub format: Option<FormatKind>,
    /// Installs a package judged risky anyway.
    pub allow_risky: bool,
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
    /// Whether the extracted package is expected to run here; `Some` for packages.
    pub compat: Option<Verdict>,
    /// The host's own package manager command for the same file, when the
    /// package is of the host's family.
    pub native_hint: Option<String>,
    /// Another asset of the release that would install without these problems.
    pub alternative: Option<Alternative>,
}

/// A portable asset of the same release, offered when a package is not
/// expected to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternative {
    pub asset: String,
    pub url: String,
    pub format: FormatKind,
}

impl InstallPlan {
    /// Why this plan must not be installed, or `None` when it may proceed.
    pub fn blocked(&self) -> Option<String> {
        let name = &self.plan.name;
        let mut message = match &self.compat {
            Some(Verdict::NeedsContainer(_)) => format!(
                "{name} needs newer system libraries than this machine has; nothing was installed"
            ),
            Some(Verdict::Risky(_)) if !self.opts.allow_risky => {
                format!("{name} may not run here")
            }
            _ => return None,
        };
        if let Some(verdict) = &self.compat {
            for finding in verdict.findings().iter().filter(|f| !f.note) {
                message.push_str("\n  - ");
                message.push_str(&finding.detail);
            }
        }
        Some(message)
    }

    /// The error to refuse this plan with: a risky plan can still be accepted,
    /// one that needs a container cannot.
    pub fn refusal(&self) -> Option<Error> {
        let message = self.blocked()?;
        Some(match self.compat {
            Some(Verdict::Risky(_)) => Error::NeedsAcceptance(message),
            _ => Error::Refused(message),
        })
    }
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
    http: Http,
    cancel: Cancel,
    progress: ProgressFn,
    host_libs: std::cell::OnceCell<crate::compat::HostLibs>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether `release` is newer than the installed `current` version of `app`.
/// GitHub compares release tags; direct URLs compare their change markers.
fn newer_release(
    app: &AppRecord,
    current: &VersionRecord,
    release: &Release,
) -> Option<UpdateInfo> {
    let (shown_current, latest) = match app.source.kind {
        SourceKind::Github => {
            let tag = release.version.as_deref()?;
            let installed = current
                .marker
                .as_deref()
                .or(current.version.as_deref())
                .unwrap_or(&current.label);
            if !is_newer(tag, installed) {
                return None;
            }
            (installed.to_string(), tag.to_string())
        }
        SourceKind::DirectUrl => {
            let fresh = release.marker.as_deref()?;
            let stored = current.marker.as_deref()?;
            if fresh == stored {
                return None;
            }
            (
                current
                    .version
                    .clone()
                    .unwrap_or_else(|| current.label.clone()),
                release.version.clone().unwrap_or_else(|| fresh.to_string()),
            )
        }
        SourceKind::LocalFile => return None,
    };
    Some(UpdateInfo {
        id: app.id.clone(),
        current: shown_current,
        latest,
    })
}

/// The command name an app is known by: the binary the package itself runs,
/// falling back to the app id when that is unknown or unusable.
fn default_command(id: &str, entry: Option<&EntryCandidate>, payload: &Payload) -> String {
    let named = match payload.format {
        FormatKind::AppImage => payload
            .desktop
            .as_deref()
            .and_then(|text| DesktopEntry::parse(text).ok())
            .and_then(|d| d.get("Exec").and_then(exec_argv0)),
        FormatKind::Tarball => entry
            .and_then(|e| e.path.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
        FormatKind::Deb | FormatKind::Rpm | FormatKind::Pacman => entry.and_then(|e| {
            payload
                .package
                .as_ref()
                .and_then(|meta| bin_link_name(&meta.index, &e.path))
                .or_else(|| e.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        }),
    };
    named
        .and_then(|n| {
            Path::new(&n)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .filter(|n| !["AppRun", "env"].contains(&n.as_str()) && validate_command(n).is_ok())
        .unwrap_or_else(|| id.to_string())
}

/// The name of a `usr/bin` or `bin` link that points at `entry`, which is what
/// the package's own users type to start it.
fn bin_link_name(index: &PayloadIndex, entry: &Path) -> Option<String> {
    index
        .links
        .keys()
        .filter(|link| {
            link.parent()
                .is_some_and(|d| d == Path::new("usr/bin") || d == Path::new("bin"))
        })
        .filter(|link| resolve_in_payload(index, link).as_deref() == Some(entry))
        .filter_map(|link| link.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .find(|n| validate_command(n).is_ok())
}

fn join_tags(tags: &std::collections::BTreeSet<&'static str>) -> String {
    tags.iter().copied().collect::<Vec<_>>().join(",")
}

fn sha256_prefix(path: &Path, cancel: &Cancel) -> Result<String> {
    let mut file = std::fs::File::open(path).at(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        cancel.check()?;
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
            http: Http::from_env()?,
            cancel: Cancel::new(),
            progress: Arc::new(|_| {}),
            host_libs: std::cell::OnceCell::new(),
        })
    }

    /// Uses `libs` instead of probing the running system.
    pub fn with_host_libs(self, libs: crate::compat::HostLibs) -> Engine {
        let cell = std::cell::OnceCell::new();
        let _ = cell.set(libs);
        Engine {
            host_libs: cell,
            ..self
        }
    }

    fn host_libs(&self) -> &crate::compat::HostLibs {
        self.host_libs.get_or_init(crate::compat::HostLibs::probe)
    }

    pub fn with_path_var(mut self, path_var: OsString) -> Engine {
        self.path_var = path_var;
        self
    }

    /// Makes downloads and installs stop with `Interrupted` once `cancel` is set.
    pub fn with_cancel(mut self, cancel: Cancel) -> Engine {
        self.cancel = cancel;
        self
    }

    /// Replaces the cancel flag used by the operations that follow.
    pub fn set_cancel(&mut self, cancel: Cancel) {
        self.cancel = cancel;
    }

    /// Reports the stages of each operation to `f`.
    pub fn with_progress(mut self, f: ProgressFn) -> Engine {
        self.progress = f;
        self
    }

    fn emit(&self, event: Progress) {
        (self.progress)(event);
    }

    /// Takes the write lock, reporting `WaitingForLock` if it has to wait.
    fn take_lock(&self) -> Result<WriteLock> {
        lock::acquire(&self.layout.data.join("lock"), || {
            self.emit(Progress::WaitingForLock)
        })
    }

    /// Takes the write lock, then recovers any operation a crashed process
    /// left unfinished. Fails if a journal cannot be recovered safely.
    fn begin(&mut self) -> Result<WriteLock> {
        let lock = self.take_lock()?;
        recover(&lock, &self.layout, &self.manifest)?;
        self.cancel.check()?;
        Ok(lock)
    }

    /// Undoes or finishes the operations that crashed processes left behind.
    pub fn recover(&mut self) -> Result<Recovered> {
        let lock = self.take_lock()?;
        recover(&lock, &self.layout, &self.manifest)
    }

    /// A transaction whose steps are journaled for `app`, which must show
    /// `expect` in the manifest once the operation has committed.
    fn journaled(&self, app: &str, expect: Expect) -> Result<Txn> {
        Txn::journaled(
            self.cancel.clone(),
            &self.layout.data.join("journal"),
            Intent {
                app: app.to_string(),
                expect,
            },
        )
    }

    /// Takes the write lock and fails if `app` is no longer what the caller
    /// read before it started working. Returns the versions and files as
    /// they are under the lock; those read earlier may be stale.
    fn lock_unchanged(&mut self, app: &AppRecord) -> Result<Locked> {
        let lock = self.begin()?;
        let changed = || {
            Error::Conflict(format!(
                "{} was changed by another unpakt process; try again",
                app.id
            ))
        };
        match self.manifest.get(&app.id)? {
            Some(now) if now.current == app.current => {}
            _ => return Err(changed()),
        }
        Ok(Locked {
            lock,
            versions: self.manifest.versions(&app.id)?,
            files: self.manifest.files(&app.id)?,
        })
    }

    pub fn with_http(mut self, http: Http) -> Engine {
        self.http = http;
        self
    }

    pub fn open_default() -> Result<Engine> {
        Engine::new(Layout::user()?, Host::detect())
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Removes what an interrupted run left under `apps/<id>`, through `txn`
    /// so a failing install puts it back. With no `known` versions the app is
    /// absent from the manifest, so the whole directory is stale. Otherwise
    /// only version and staging directories the manifest does not list go.
    /// Backups and trash of a transaction (`.unpakt-*`) are never touched,
    /// since they may hold original files.
    fn clear_stale(&self, id: &str, known: &[&str], txn: &mut Txn) -> Result<()> {
        validate_command(id)?;
        let app_dir = self.layout.apps.join(id);
        let Ok(meta) = std::fs::symlink_metadata(&app_dir) else {
            return Ok(());
        };
        if !meta.is_dir() {
            return Ok(());
        }
        if known.is_empty() {
            tracing::warn!("removing leftover {}", app_dir.display());
            return txn.remove_dir_all(&app_dir);
        }
        let mut stale = Vec::new();
        for entry in std::fs::read_dir(&app_dir).at(&app_dir)? {
            let entry = entry.at(&app_dir)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().at(entry.path())?.is_dir();
            let ours = name.starts_with(".staging-") || !name.starts_with('.');
            if is_dir && ours && name != "current" && !known.contains(&name.as_str()) {
                stale.push(entry.path());
            }
        }
        stale.sort();
        for dir in stale {
            tracing::warn!("removing leftover {}", dir.display());
            txn.remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Leftover download directories older than this are removed.
    const STALE_DOWNLOAD_AGE: Duration = Duration::from_secs(24 * 3600);

    fn sweep_old_downloads(root: &Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let old = entry.file_name().to_string_lossy().starts_with("dl-")
                && entry.file_type().is_ok_and(|t| t.is_dir())
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Self::STALE_DOWNLOAD_AGE);
            if old && let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!("cannot remove old download {}: {e}", entry.path().display());
            }
        }
    }

    /// Downloads into a fresh directory under the cache, so equal file names
    /// from different URLs never collide. Downloads are not installed state.
    fn download_asset(&self, url: &str) -> Result<PathBuf> {
        let root = self.layout.cache.join("downloads");
        std::fs::create_dir_all(&root).at(&root)?;
        Self::sweep_old_downloads(&root);
        let dir = tempfile::Builder::new()
            .prefix("dl-")
            .tempdir_in(&root)
            .at(&root)?
            .keep();
        let guard = DownloadGuard(Some(dir.clone()));
        let file = Url::parse(url)
            .ok()
            .and_then(|u| {
                u.path_segments()
                    .and_then(|mut s| s.next_back().map(String::from))
            })
            .unwrap_or_default();
        // At most one event per interval, plus the last chunk's.
        let mut last_emit: Option<Instant> = None;
        let mut pending: Option<(u64, Option<u64>)> = None;
        let path = self
            .http
            .download_with(url, &dir, &self.cancel, &mut |done, total| {
                if last_emit.is_none_or(|t| t.elapsed() >= DOWNLOAD_EVENT_INTERVAL) {
                    last_emit = Some(Instant::now());
                    pending = None;
                    self.emit(Progress::Downloading {
                        file: file.clone(),
                        done,
                        total,
                    });
                } else {
                    pending = Some((done, total));
                }
            })?
            .path;
        if let Some((done, total)) = pending {
            self.emit(Progress::Downloading { file, done, total });
        }
        guard.keep();
        Ok(path)
    }

    pub fn inspect(&self, input: &str, opts: AddOpts) -> Result<InstallPlan> {
        if let Some(command) = &opts.command {
            validate_command(command)?;
        }
        let (plugin, source) = resolve(input)?;
        let pattern = opts
            .pattern
            .as_deref()
            .map(Regex::new)
            .transpose()
            .map_err(|e| Error::Parse(format!("invalid --pattern: {e}")))?;
        if source.kind != SourceKind::LocalFile {
            self.emit(Progress::Fetching);
        }
        let release = plugin.latest(&source, &self.http)?;
        let asset = choose(&release.assets, &self.host, pattern.as_ref(), opts.format)?;
        let canonical = match source.kind {
            SourceKind::LocalFile => PathBuf::from(&asset.url),
            _ => self.download_asset(&asset.url)?,
        };
        let download = self.download_guard(&canonical);
        let file_name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let artifact = Artifact {
            path: canonical.clone(),
            file_name,
        };
        self.emit(Progress::Inspecting);
        self.cancel.check()?;
        let payload = detect(&artifact)?.inspect(&artifact)?;
        if let Some(wanted) = opts.format
            && payload.format != wanted
        {
            return Err(Error::Refused(format!(
                "{} is a {} file, not {}",
                artifact.file_name,
                payload.format.as_str(),
                wanted.as_str()
            )));
        }
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

        let label_version = match source.kind {
            SourceKind::LocalFile => payload.version.as_deref(),
            SourceKind::DirectUrl | SourceKind::Github => release.version.as_deref(),
        };
        let label = match label_version {
            Some(v) => version_label(v),
            None => sha256_prefix(&canonical, &self.cancel)?,
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
            Some(default_command(&id, entry.as_ref(), &payload))
        };

        let compat = payload.package.as_ref().map(|meta| {
            let path = entry.as_ref().map(|e| e.path.clone()).unwrap_or_default();
            check_compat(meta, &path, self.host_libs())
        });
        let native_hint = payload
            .package
            .as_ref()
            .filter(|meta| Some(meta.family) == self.host.family())
            .map(|meta| match source.kind {
                SourceKind::LocalFile => native_install_hint(meta.family, &canonical),
                _ => native_download_hint(meta.family, &asset.url, &asset.name),
            });
        let alternative = if payload.package.is_some() {
            [FormatKind::AppImage, FormatKind::Tarball]
                .into_iter()
                .find_map(|kind| {
                    choose(&release.assets, &self.host, pattern.as_ref(), Some(kind))
                        .ok()
                        .map(|a| Alternative {
                            asset: a.name.clone(),
                            url: a.url.clone(),
                            format: kind,
                        })
                })
        } else {
            None
        };

        let plan = Plan {
            id,
            name: payload.name.clone(),
            artifact,
            payload,
            label,
            entry,
        };
        self.cancel.check()?;
        download.keep();
        Ok(InstallPlan {
            plan,
            source,
            marker: release.marker,
            backend: backend.id(),
            want_desktop,
            command,
            opts,
            compat,
            native_hint,
            alternative,
        })
    }

    /// A guard for the download directory holding `artifact`; inert for files
    /// that were not downloaded by this engine.
    fn download_guard(&self, artifact: &Path) -> DownloadGuard {
        let root = self.layout.cache.join("downloads");
        DownloadGuard(
            artifact
                .parent()
                .filter(|dir| dir.parent() == Some(root.as_path()))
                .map(Path::to_path_buf),
        )
    }

    /// Drops a plan that will not be installed, deleting its download.
    pub fn discard(&self, plan: InstallPlan) {
        drop(self.download_guard(&plan.plan.artifact.path));
    }

    pub fn install(&mut self, p: InstallPlan) -> Result<AppRecord> {
        let _download = self.download_guard(&p.plan.artifact.path);
        if let Some(error) = p.refusal() {
            return Err(error);
        }
        let id = p.plan.id.clone();
        let _lock = self.begin()?;
        if self.manifest.get(&id)?.is_some() {
            return Err(Error::Conflict(format!(
                "{id} is already installed; use `unpakt update {id}`"
            )));
        }
        let backend = for_payload(&p.plan.payload)
            .ok_or_else(|| Error::Unsupported("no backend can install this package".into()))?;

        self.emit(Progress::Installing);
        let mut txn = self.journaled(&id, Expect::Current(p.plan.label.clone()))?;
        self.clear_stale(&id, &[], &mut txn)?;
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
        let format_opt = p.opts.format.map(|f| f.as_str());
        if let Some(format) = format_opt {
            options.push(("format", format));
        }
        let accepted = match &p.compat {
            Some(verdict @ Verdict::Risky(_)) if p.opts.allow_risky => {
                Some(join_tags(&verdict.risk_tags()))
            }
            _ => None,
        };
        if let Some(accepted) = &accepted {
            options.push(("compat.accepted", accepted));
        }
        txn.cancel().check()?;
        self.manifest
            .commit_install_with(&app, &version, &integration.files, &options, &[])?;
        txn.commit();
        self.refresh_desktop_database();
        Ok(app)
    }

    /// Apps with a newer release than the installed one. Local-file apps have
    /// no origin to ask and are skipped. Apps that could not be checked are
    /// reported in `failed`, never as up to date.
    pub fn check_updates(&self, id: Option<&str>) -> Result<UpdateCheck> {
        let apps = match id {
            Some(id) => vec![self.get(id)?.0],
            None => self.manifest.list()?,
        };
        let mut check = UpdateCheck::default();
        for app in apps {
            if app.source.kind == SourceKind::LocalFile {
                continue;
            }
            match self.check_app_update(&app) {
                Ok(Some(info)) => check.updates.push(info),
                Ok(None) => {}
                Err(e) => check.failed.push((app.id.clone(), e)),
            }
        }
        Ok(check)
    }

    fn check_app_update(&self, app: &AppRecord) -> Result<Option<UpdateInfo>> {
        let current = self.current_version(app)?;
        let release = for_kind(app.source.kind).latest(&app.source, &self.http)?;
        Ok(newer_release(app, &current, &release))
    }

    fn current_version(&self, app: &AppRecord) -> Result<VersionRecord> {
        self.manifest
            .versions(&app.id)?
            .into_iter()
            .find(|v| v.label == app.current)
            .ok_or_else(|| Error::NotFound(format!("{} {}", app.id, app.current)))
    }

    /// Installs the latest release next to the current version and switches to
    /// it. Returns the new label, or `None` when already up to date. Any
    /// failure leaves the current version exactly as it was.
    pub fn update(&mut self, id: &str) -> Result<Option<String>> {
        self.update_with(id, false)
    }

    /// Like [`Engine::update`]. With `allow_risky`, compatibility problems the
    /// installed version had not accepted are accepted too.
    pub fn update_with(&mut self, id: &str, allow_risky: bool) -> Result<Option<String>> {
        let (app, versions, _) = self.get(id)?;
        if app.source.kind == SourceKind::LocalFile {
            return Err(Error::Unsupported(format!(
                "{id} was installed from a local file and has no update source"
            )));
        }
        let current = self.current_version(&app)?;
        let release = for_kind(app.source.kind).latest(&app.source, &self.http)?;
        let newer = newer_release(&app, &current, &release);
        if app.source.kind == SourceKind::Github && newer.is_none() {
            return Ok(None);
        }
        // The release may already be installed (after a rollback): switch to
        // it instead of fetching it again.
        if newer.is_some()
            && let Some(version) = release.version.as_deref()
        {
            let label = version_label(version);
            if let Some(target) = versions.iter().find(|v| v.label == label)
                && label != current.label
            {
                let locked = self.lock_unchanged(&app)?;
                let target = locked
                    .versions
                    .iter()
                    .find(|v| v.label == target.label)
                    .ok_or_else(|| {
                        Error::Conflict(format!(
                            "{id} was changed by another unpakt process; try again"
                        ))
                    })?;
                self.activate(&app, target, &locked.files)?;
                return Ok(Some(label));
            }
        }

        let entry = self.manifest.option(id, "entry")?.map(PathBuf::from);
        let pattern = self
            .manifest
            .option(id, "pattern")?
            .or_else(|| app.source.pattern.clone());
        let opts = AddOpts {
            id: Some(id.to_string()),
            entry,
            command: app.command.clone(),
            desktop: Some(app.desktop),
            no_bin: app.command.is_none(),
            force: false,
            pattern,
            format: Some(app.format),
            allow_risky: false,
        };
        let mut p = self.inspect(&app.source.location, opts)?;
        let _download = self.download_guard(&p.plan.artifact.path);
        let label = p.plan.label.clone();
        if let Some(target) = versions.iter().find(|v| v.label == label) {
            let fetched = Release {
                version: p.plan.payload.version.clone(),
                marker: p.marker.clone(),
                assets: Vec::new(),
            };
            if label != current.label && newer_release(&app, &current, &fetched).is_some() {
                let locked = self.lock_unchanged(&app)?;
                let target = locked
                    .versions
                    .iter()
                    .find(|v| v.label == target.label)
                    .ok_or_else(|| {
                        Error::Conflict(format!(
                            "{id} was changed by another unpakt process; try again"
                        ))
                    })?;
                self.activate(&app, target, &locked.files)?;
                return Ok(Some(label));
            }
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
            if newer_release(&app, &current, &fetched).is_none() {
                return Ok(None);
            }
        }
        let mut accepted_tags: Option<std::collections::BTreeSet<String>> = None;
        match &p.compat {
            Some(Verdict::NeedsContainer(_)) => {
                let message = p.blocked().unwrap_or_default();
                return Err(Error::Refused(format!("{message}; current version kept")));
            }
            Some(verdict @ Verdict::Risky(_)) => {
                let mut accepted: std::collections::BTreeSet<String> = self
                    .manifest
                    .option(id, "compat.accepted")?
                    .map(|text| {
                        text.split(',')
                            .filter(|t| !t.is_empty())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let new: Vec<&str> = verdict
                    .risk_tags()
                    .into_iter()
                    .filter(|t| !accepted.contains(*t))
                    .collect();
                if !new.is_empty() && !allow_risky {
                    return Err(Error::NeedsAcceptance(format!(
                        "{id} {label} adds new compatibility problems ({}); current version kept",
                        new.join(", ")
                    )));
                }
                p.opts.allow_risky = true;
                accepted.extend(new.into_iter().map(String::from));
                accepted_tags = Some(accepted);
            }
            _ => {}
        }
        let backend = for_payload(&p.plan.payload)
            .ok_or_else(|| Error::Unsupported("no backend can install this package".into()))?;

        let Locked {
            lock: _lock,
            versions,
            files,
        } = self.lock_unchanged(&app)?;
        self.emit(Progress::Installing);
        let mut txn = self.journaled(id, Expect::Current(label.clone()))?;
        let known: Vec<&str> = versions.iter().map(|v| v.label.as_str()).collect();
        self.clear_stale(id, &known, &mut txn)?;
        let installed = backend.install(&p.plan, &self.layout, &mut txn)?;
        let link = self.layout.apps.join(id).join("current");
        txn.symlink_swap(Path::new(&label), &link)?;
        let exec = link.join(&installed.entry);
        let integration = integrate(
            &self.layout,
            &IntegrateReq {
                id,
                name: &p.plan.name,
                exec: &exec,
                kind: installed.kind,
                desktop_src: p.plan.payload.desktop.as_deref(),
                icon: p.plan.payload.icon.as_ref(),
                want_desktop: app.desktop,
                command: app.command.as_deref(),
                force: false,
                no_fuse_appimage: p.plan.payload.format == FormatKind::AppImage && !self.host.fuse2,
                owned: &files,
                path_var: &self.path_var,
            },
            &mut txn,
        )?;
        for warning in integration.warnings.iter().chain(&p.plan.payload.warnings) {
            tracing::warn!("{id}: {warning}");
        }

        // `versions` is newest first; the new version takes the first slot.
        let pruned: Vec<&VersionRecord> = versions
            .iter()
            .skip(KEEP_VERSIONS - 1)
            .filter(|v| v.label != label)
            .collect();
        for v in &pruned {
            let dir = self.layout.apps.join(id).join(&v.label);
            if std::fs::symlink_metadata(&dir).is_ok() {
                txn.remove_dir_all(&dir)?;
            }
        }

        let pruned_labels: Vec<&str> = pruned.iter().map(|v| v.label.as_str()).collect();
        let record = AppRecord {
            id: id.to_string(),
            name: p.plan.name.clone(),
            format: p.plan.payload.format,
            backend: p.backend.to_string(),
            source: app.source.clone(),
            current: label.clone(),
            desktop: app.desktop,
            command: app.command.clone(),
        };
        let version = VersionRecord {
            label: label.clone(),
            version: p.plan.payload.version.clone(),
            marker: p.marker.clone(),
            entry: installed.entry,
            exec_kind: installed.kind,
            installed_at: now(),
        };
        let accepted_text = accepted_tags.map(|tags| {
            tags.iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(",")
        });
        let options: Vec<(&str, &str)> = accepted_text
            .as_deref()
            .map(|text| ("compat.accepted", text))
            .into_iter()
            .collect();
        txn.cancel().check()?;
        self.manifest.commit_install_with(
            &record,
            &version,
            &integration.files,
            &options,
            &pruned_labels,
        )?;
        txn.commit();
        self.refresh_desktop_database();
        Ok(Some(label))
    }

    /// Makes the next-older installed version current again. Nothing is deleted.
    pub fn rollback(&mut self, id: &str) -> Result<String> {
        let _lock = self.begin()?;
        let (app, versions, files) = self.get(id)?;
        let pos = versions
            .iter()
            .position(|v| v.label == app.current)
            .ok_or_else(|| Error::NotFound(format!("{id} {}", app.current)))?;
        let target = versions
            .get(pos + 1)
            .ok_or_else(|| Error::NotFound(format!("{id} has no older version to roll back to")))?;
        self.activate(&app, target, &files)?;
        Ok(target.label.clone())
    }

    /// Points `current` at the already installed `target` and re-integrates
    /// it. The launcher entry and icon stay as installed; only what they run
    /// changes.
    fn activate(
        &mut self,
        app: &AppRecord,
        target: &VersionRecord,
        files: &[FileRecord],
    ) -> Result<()> {
        let id = app.id.as_str();
        let version_dir = self.layout.apps.join(id).join(&target.label);
        if std::fs::symlink_metadata(&version_dir).is_err() {
            return Err(Error::NotFound(format!(
                "{} is missing on disk",
                version_dir.display()
            )));
        }

        self.emit(Progress::Installing);
        let desktop_src = files
            .iter()
            .find(|f| f.kind == FileKind::Desktop)
            .and_then(|f| std::fs::read_to_string(&f.path).ok());
        let icon = files
            .iter()
            .find(|f| f.kind == FileKind::Icon)
            .and_then(|f| {
                Some(Icon {
                    ext: f.path.extension()?.to_string_lossy().into_owned(),
                    data: std::fs::read(&f.path).ok()?,
                })
            });

        let mut txn = self.journaled(id, Expect::Current(target.label.clone()))?;
        let link = self.layout.apps.join(id).join("current");
        txn.symlink_swap(Path::new(&target.label), &link)?;
        let exec = link.join(&target.entry);
        let integration = integrate(
            &self.layout,
            &IntegrateReq {
                id,
                name: &app.name,
                exec: &exec,
                kind: target.exec_kind,
                desktop_src: desktop_src.as_deref(),
                icon: icon.as_ref(),
                want_desktop: app.desktop,
                command: app.command.as_deref(),
                force: false,
                no_fuse_appimage: app.format == FormatKind::AppImage && !self.host.fuse2,
                owned: files,
                path_var: &self.path_var,
            },
            &mut txn,
        )?;
        self.cancel.check()?;
        self.manifest
            .set_current(id, &target.label, &integration.files)?;
        txn.commit();
        self.refresh_desktop_database();
        Ok(())
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
        let _lock = self.begin()?;
        let (_, _, files) = self.get(id)?;
        self.emit(Progress::Installing);
        let mut txn = self.journaled(id, Expect::Absent)?;
        disintegrate(&files, &mut txn)?;
        let dir = self.layout.apps.join(id);
        if std::fs::symlink_metadata(&dir).is_ok() {
            txn.remove_dir_all(&dir)?;
        }
        self.cancel.check()?;
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

        checks.push(self.check_journal());

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

    fn check_journal(&self) -> Check {
        let name = "journal".to_string();
        match journals(&self.layout.data.join("journal")) {
            Ok(files) if files.is_empty() => Check {
                name,
                ok: true,
                detail: "no unfinished operations".into(),
            },
            Ok(files) => {
                let list: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
                Check {
                    name,
                    ok: false,
                    detail: format!(
                        "unfinished operations are recorded in {}; the next change tries to recover them and is refused until they are repaired or removed",
                        list.join(", ")
                    ),
                }
            }
            Err(e) => Check {
                name,
                ok: false,
                detail: format!("cannot read the journal directory: {e}"),
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn big_file(dir: &Path) -> PathBuf {
        let path = dir.join("big.bin");
        std::fs::write(&path, vec![7u8; 200 * 1024]).unwrap();
        path
    }

    #[test]
    fn sha256_prefix_stops_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = big_file(dir.path());
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(matches!(
            sha256_prefix(&path, &cancel),
            Err(Error::Interrupted)
        ));
    }

    #[test]
    fn sha256_prefix_returns_twelve_hex_chars() {
        let dir = tempfile::tempdir().unwrap();
        let path = big_file(dir.path());
        let prefix = sha256_prefix(&path, &Cancel::new()).unwrap();
        assert_eq!(prefix.len(), 12);
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
