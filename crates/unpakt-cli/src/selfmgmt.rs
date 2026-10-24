//! `unpakt self update` and `unpakt self uninstall`: unpakt updating and
//! removing itself the way it was installed.

use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use semver::Version;
use unpakt_core::engine::Engine;
use unpakt_core::http::Http;
use unpakt_core::paths::Layout;

use crate::confirm;

const DEFAULT_REPO_URL: &str = "https://github.com/krotrn/unpakt";
const INSTALL_DOCS: &str = "https://github.com/krotrn/unpakt#install";
const APP_ID: &str = "io.github.krotrn.Unpakt";
const PACMAN_CONF: &str = "/etc/pacman.conf";

#[derive(Subcommand)]
pub enum SelfAction {
    /// Update unpakt itself to the latest release
    Update {
        /// Only print the current and the latest version
        #[arg(long)]
        check: bool,
        /// Update without asking for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove unpakt itself
    Uninstall {
        /// Also remove every installed app and unpakt's data, config and cache
        #[arg(long)]
        purge: bool,
        /// Uninstall without asking for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

pub fn run(action: SelfAction) -> Result<()> {
    match action {
        SelfAction::Update { check, yes } => update(check, yes),
        SelfAction::Uninstall { purge, yes } => uninstall(purge, yes),
    }
}

// ── Install routes ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    User,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Package {
    Pacman,
    Deb,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    /// `managed` is set when the file sits where get.sh puts it.
    AppImage {
        path: PathBuf,
        managed: Option<Scope>,
    },
    Pacman,
    Deb,
    /// Built by install.sh into `prefix`.
    Source {
        prefix: PathBuf,
    },
    Cargo,
    /// The static command-line binary get.sh installs.
    Binary(Scope),
    Unknown,
}

/// Where the running unpakt and its surroundings are.
struct Host {
    exe: PathBuf,
    appimage: Option<PathBuf>,
    home: PathBuf,
    cargo_home: PathBuf,
    xdg_data: PathBuf,
    xdg_config: PathBuf,
}

/// The AppImage this process runs from. `APPIMAGE` alone can be left over in a
/// shell that once ran one, so the executable must also sit inside `APPDIR`.
fn appimage_in_use(
    appimage: Option<PathBuf>,
    appdir: Option<PathBuf>,
    exe: &Path,
) -> Option<PathBuf> {
    appimage.filter(|_| appdir.is_some_and(|dir| exe.starts_with(dir)))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

impl Host {
    fn from_env() -> Result<Host> {
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("cannot locate the running unpakt")?;
        // Same root the engine uses, so tests and UNPAKT_ROOT stay contained.
        let home = env_path("UNPAKT_ROOT")
            .or_else(|| env_path("HOME"))
            .context("cannot find the home directory")?;
        let home = home.canonicalize().unwrap_or(home);
        Ok(Host {
            exe: exe.clone(),
            appimage: appimage_in_use(
                env_path("APPIMAGE"),
                env_path("APPDIR").and_then(|d| d.canonicalize().ok()),
                &exe,
            ),
            cargo_home: env_path("CARGO_HOME").unwrap_or_else(|| home.join(".cargo")),
            xdg_data: env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
            xdg_config: env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            home,
        })
    }

    fn lib_appimage(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::User => self.home.join(".local/lib/unpakt/unpakt.AppImage"),
            Scope::System => PathBuf::from("/usr/local/lib/unpakt/unpakt.AppImage"),
        }
    }

    fn bin_dir(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::User => self.home.join(".local/bin"),
            Scope::System => PathBuf::from("/usr/local/bin"),
        }
    }

    fn share_dir(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::User => self.xdg_data.clone(),
            Scope::System => PathBuf::from("/usr/local/share"),
        }
    }

    /// The shell completions get.sh writes for `scope`.
    fn completion_files(&self, scope: Scope) -> Vec<PathBuf> {
        let share = self.share_dir(scope);
        match scope {
            Scope::User => vec![
                share.join("bash-completion/completions/unpakt"),
                self.xdg_config.join("fish/completions/unpakt.fish"),
            ],
            Scope::System => vec![
                share.join("bash-completion/completions/unpakt"),
                share.join("zsh/site-functions/_unpakt"),
                share.join("fish/vendor_completions.d/unpakt.fish"),
            ],
        }
    }

    fn launcher_files(&self, scope: Scope) -> Vec<PathBuf> {
        let share = self.share_dir(scope);
        vec![
            share.join(format!("applications/{APP_ID}.desktop")),
            share.join(format!("icons/hicolor/scalable/apps/{APP_ID}.svg")),
        ]
    }
}

/// How the running unpakt was installed. `owner` says which package, if any,
/// owns a file.
fn detect(host: &Host, owner: &dyn Fn(&Path) -> Option<Package>) -> Route {
    if let Some(path) = &host.appimage {
        let managed = [Scope::User, Scope::System]
            .into_iter()
            .find(|s| *path == host.lib_appimage(*s));
        return Route::AppImage {
            path: path.clone(),
            managed,
        };
    }
    match owner(&host.exe) {
        Some(Package::Pacman) => return Route::Pacman,
        Some(Package::Deb) => return Route::Deb,
        None => {}
    }
    if host.exe.file_name() == Some(OsStr::new("unpakt"))
        && let Some(bin) = host.exe.parent()
        && bin.file_name() == Some(OsStr::new("bin"))
        && let Some(prefix) = bin.parent()
        && prefix.join("lib/unpakt/install.sh").is_file()
    {
        return Route::Source {
            prefix: prefix.to_path_buf(),
        };
    }
    if host.exe.starts_with(host.cargo_home.join("bin")) {
        return Route::Cargo;
    }
    if let Some(scope) = [Scope::User, Scope::System]
        .into_iter()
        .find(|s| host.exe == host.bin_dir(*s).join("unpakt"))
        && fs::symlink_metadata(&host.exe).is_ok_and(|m| m.is_file())
    {
        return Route::Binary(scope);
    }
    Route::Unknown
}

/// Asks the system's package managers; both queries are read-only.
fn package_owner(path: &Path) -> Option<Package> {
    let owns = |program: &str, flag: &str| {
        Command::new(program)
            .arg(flag)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if owns("pacman", "-Qo") {
        Some(Package::Pacman)
    } else if owns("dpkg", "-S") {
        Some(Package::Deb)
    } else {
        None
    }
}

// ── Latest release ───────────────────────────────────────────────────────────

struct Sum {
    sha256: String,
    file: String,
}

/// The lines of a `sha256sum` file: `<hash>  <file>`, with `*` before the file
/// name for binary mode.
fn parse_sums(text: &str) -> Vec<Sum> {
    text.lines()
        .filter_map(|line| {
            let (hash, file) = line.split_once(char::is_whitespace)?;
            let file = file.trim_start().trim_start_matches('*');
            (!hash.is_empty() && !file.is_empty()).then(|| Sum {
                sha256: hash.to_ascii_lowercase(),
                file: file.to_string(),
            })
        })
        .collect()
}

/// The release version named by the static binary in `sums`.
fn latest_version(sums: &[Sum]) -> Option<Version> {
    sums.iter().find_map(|s| {
        let v = s
            .file
            .strip_prefix("unpakt-")?
            .strip_suffix("-x86_64-linux-musl")?;
        Version::parse(v).ok()
    })
}

fn is_newer(latest: &Version, current: &Version) -> bool {
    latest > current
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semver")
}

fn repo_url() -> String {
    // Tests point this at a local server; a release build always uses GitHub.
    let over = std::env::var("UNPAKT_REPO_URL")
        .ok()
        .filter(|v| cfg!(debug_assertions) && !v.is_empty());
    over.unwrap_or_else(|| DEFAULT_REPO_URL.into())
        .trim_end_matches('/')
        .to_string()
}

struct Latest {
    version: Version,
    sums: Vec<Sum>,
}

impl Latest {
    fn fetch(http: &Http, repo: &str, tmp: &Path) -> Result<Latest> {
        let url = format!("{repo}/releases/latest/download/SHA256SUMS");
        let file = http.download(&url, tmp)?;
        let sums = parse_sums(&fs::read_to_string(&file.path).context("reading SHA256SUMS")?);
        let version = latest_version(&sums)
            .context("the latest release has no command-line binary to take a version from")?;
        Ok(Latest { version, sums })
    }

    /// Downloads release file `name` into `dir` and checks it against SHA256SUMS.
    fn fetch_asset(&self, http: &Http, repo: &str, name: &str, dir: &Path) -> Result<PathBuf> {
        let expected = self
            .sums
            .iter()
            .find(|s| s.file == name)
            .with_context(|| format!("the latest release has no {name}"))?;
        let url = format!("{repo}/releases/download/v{}/{name}", self.version);
        let download = http.download(&url, dir)?;
        if download.sha256 != expected.sha256 {
            let _ = fs::remove_file(&download.path);
            bail!("{name} failed its checksum; nothing was changed");
        }
        Ok(download.path)
    }

    fn binary_name(&self) -> String {
        format!("unpakt-{}-x86_64-linux-musl", self.version)
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// `program`, run through sudo unless we already are root.
fn privileged(program: &str) -> Command {
    if is_root() {
        Command::new(program)
    } else {
        let mut cmd = Command::new("sudo");
        cmd.arg(program);
        cmd
    }
}

/// Runs `cmd` with the terminal inherited, so sudo can ask for a password.
fn run_inherited(cmd: &mut Command) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let status = cmd
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!("{program} failed ({status})");
    }
    Ok(())
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Writes `src` over `dest` through a temp file beside it, so the swap is one
/// rename and a running `dest` is never half-written.
fn replace_file(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("no directory to write into")?;
    let tmp = tempfile::Builder::new()
        .prefix(".unpakt-update-")
        .tempfile_in(dir)
        .with_context(|| format!("cannot write in {} (try as root)", dir.display()))?;
    fs::copy(src, tmp.path())?;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755))?;
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(dir.as_os_str().as_bytes())
        // SAFETY: the pointer is a valid NUL-terminated string for the call.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

/// Whether replacing `dest` needs sudo. Errors when it does and `scope` is not
/// one the installer put under /usr/local, so nothing is downloaded in vain.
fn needs_sudo_to_place(dest: &Path, scope: Option<Scope>) -> Result<bool> {
    let dir = dest.parent().context("no directory to write into")?;
    if writable(dir) {
        return Ok(false);
    }
    match scope {
        Some(Scope::System) if !is_root() => Ok(true),
        _ => bail!("cannot write in {} (try as root)", dir.display()),
    }
}

/// Puts `src` at `dest`: a rename for a writable directory, else `install` via sudo.
fn place_file(src: &Path, dest: &Path, sudo: bool) -> Result<()> {
    if sudo {
        run_inherited(privileged("install").arg("-m755").arg(src).arg(dest))
    } else {
        replace_file(src, dest)
    }
}

/// The shell a completion file written by get.sh is for.
fn completion_shell(file: &Path) -> Option<&'static str> {
    let text = file.to_string_lossy();
    if text.ends_with(".fish") {
        Some("fish")
    } else if text.contains("/zsh/") {
        Some("zsh")
    } else if text.contains("/bash-completion/") {
        Some("bash")
    } else {
        None
    }
}

/// Rewrites the completion files that exist with the ones from `exe`, so they
/// match the new version. Failures leave the old file in place.
fn refresh_completions(exe: &Path, files: &[PathBuf]) {
    for file in files.iter().filter(|f| f.is_file()) {
        let Some(shell) = completion_shell(file) else {
            continue;
        };
        let out = Command::new(exe)
            .args(["completion", shell])
            .env("APPIMAGE_EXTRACT_AND_RUN", "1")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        if let Ok(out) = out
            && out.status.success()
            && !out.stdout.is_empty()
        {
            let tmp = file.with_extension("unpakt-new");
            if fs::write(&tmp, &out.stdout).is_err() || fs::rename(&tmp, file).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        }
    }
}

/// Unpacks a gzip tarball into `dest` and returns its single top-level
/// directory. Only regular files and directories under relative paths are
/// accepted.
fn unpack_source(archive: &Path, dest: &Path) -> Result<PathBuf> {
    let file = fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        }
        let path = entry.path()?.into_owned();
        if !path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        {
            bail!("the source archive has an unsafe path: {}", path.display());
        }
        if !(kind.is_file() || kind.is_dir()) {
            bail!(
                "the source archive has an entry that is neither file nor directory: {}",
                path.display()
            );
        }
        if !entry.unpack_in(dest)? {
            bail!("the source archive has an unsafe path: {}", path.display());
        }
    }
    let mut tops = fs::read_dir(dest)?.collect::<std::io::Result<Vec<_>>>()?;
    match tops.as_mut_slice() {
        [only] if only.path().is_dir() => Ok(only.path()),
        _ => bail!("the source archive does not have a single top-level directory"),
    }
}

/// `text` without its `[unpakt]` section: from the header up to, not
/// including, the next section header or blank line.
fn without_pacman_repo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if skipping {
            if trimmed.is_empty() || trimmed.starts_with('[') {
                skipping = false;
            } else {
                continue;
            }
        }
        if trimmed == "[unpakt]" {
            skipping = true;
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Removes `paths` that exist. System files go through sudo for non-root users.
fn remove_paths(paths: &[PathBuf], scope: Scope) -> Result<()> {
    let present: Vec<&PathBuf> = paths
        .iter()
        .filter(|p| fs::symlink_metadata(p).is_ok())
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    if scope == Scope::System && !is_root() {
        run_inherited(privileged("rm").arg("-f").arg("--").args(&present))?;
    } else {
        for path in &present {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot remove {}", path.display()));
                }
            }
        }
    }
    for path in present {
        println!("removed {}", path.display());
    }
    Ok(())
}

fn unknown_route() -> anyhow::Error {
    anyhow::anyhow!("cannot tell how this unpakt was installed; see {INSTALL_DOCS}")
}

// ── update ───────────────────────────────────────────────────────────────────

fn update(check: bool, yes: bool) -> Result<()> {
    let host = Host::from_env()?;
    let route = detect(&host, &package_owner);
    if route == Route::Unknown && !check {
        return Err(unknown_route());
    }
    let tmp = tempfile::tempdir()?;
    // Apt reads the .deb as its own user.
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755))?;
    let http = Http::from_env()?;
    let repo = repo_url();
    let latest = Latest::fetch(&http, &repo, tmp.path())?;
    let current = current_version();
    println!("current: {current}");
    println!("latest:  {}", latest.version);
    if check {
        return Ok(());
    }
    if !is_newer(&latest.version, &current) {
        println!("already the latest");
        return Ok(());
    }
    let version = &latest.version;
    match route {
        Route::Pacman => {
            if !yes {
                confirm(
                    "update",
                    "This runs `sudo pacman -Syu unpakt`, which upgrades every package on the system \
                     (Arch does not support partial upgrades). Continue?",
                )?;
            }
            run_inherited(privileged("pacman").args(["-Syu", "unpakt"]))?;
        }
        Route::Deb => {
            let deb = format!("unpakt_{version}-1_amd64.deb");
            let path = latest.fetch_asset(&http, &repo, &deb, tmp.path())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
            if !yes {
                confirm("update", &format!("Install {deb} with `sudo apt-get`?"))?;
            }
            run_inherited(privileged("apt-get").arg("update"))?;
            run_inherited(privileged("apt-get").args(["install", "-y"]).arg(&path))?;
        }
        Route::AppImage { path, managed } => {
            let sudo = needs_sudo_to_place(&path, managed)?;
            if sudo && !yes {
                confirm(
                    "update",
                    &format!("Replace {} with `sudo install`?", path.display()),
                )?;
            }
            let name = format!("unpakt-{version}-x86_64.AppImage");
            let new = latest.fetch_asset(&http, &repo, &name, tmp.path())?;
            place_file(&new, &path, sudo)?;
            if let Some(scope) = managed {
                refresh_completions(&path, &host.completion_files(scope));
            }
            println!("updated {} to {version}", path.display());
        }
        Route::Binary(scope) => {
            let sudo = needs_sudo_to_place(&host.exe, Some(scope))?;
            if sudo && !yes {
                confirm(
                    "update",
                    &format!("Replace {} with `sudo install`?", host.exe.display()),
                )?;
            }
            let new = latest.fetch_asset(&http, &repo, &latest.binary_name(), tmp.path())?;
            place_file(&new, &host.exe, sudo)?;
            refresh_completions(&host.exe, &host.completion_files(scope));
            println!("updated {} to {version}", host.exe.display());
        }
        Route::Source { prefix } => {
            if !on_path("cargo") {
                bail!(
                    "cargo not found: updating a source install builds it; install Rust (https://rustup.rs)"
                );
            }
            if !yes {
                confirm(
                    "update",
                    &format!(
                        "Build unpakt {version} from source and install it into {}?",
                        prefix.display()
                    ),
                )?;
            }
            let url = format!("{repo}/archive/refs/tags/v{version}.tar.gz");
            // The build is large; /tmp is often a small tmpfs.
            let cache = Layout::user()?.cache;
            fs::create_dir_all(&cache)?;
            let work = tempfile::Builder::new()
                .prefix("self-update-")
                .tempdir_in(&cache)?;
            let archive = http.download(&url, work.path())?;
            let unpacked = work.path().join("src");
            fs::create_dir(&unpacked)?;
            let tree = unpack_source(&archive.path, &unpacked)?;
            let mut cmd = Command::new("bash");
            cmd.arg(tree.join("install.sh")).env("PREFIX", &prefix);
            if !prefix.join("bin/unpakt-gui").exists() {
                cmd.arg("--cli-only");
            }
            run_inherited(&mut cmd)?;
        }
        Route::Cargo => {
            println!("unpakt was installed with cargo; update it with:");
            println!("  cargo install --locked unpakt-cli");
        }
        Route::Unknown => unreachable!("refused before the download"),
    }
    Ok(())
}

// ── uninstall ────────────────────────────────────────────────────────────────

fn uninstall(purge: bool, yes: bool) -> Result<()> {
    let host = Host::from_env()?;
    let route = detect(&host, &package_owner);
    if route == Route::Unknown {
        return Err(unknown_route());
    }
    let instructions_only = matches!(route, Route::Cargo | Route::AppImage { managed: None, .. });
    if !yes && (purge || !instructions_only) {
        let what = if purge {
            "Remove unpakt, every app it installed, and its data, config and cache?"
        } else {
            "Remove unpakt? (installed apps are kept)"
        };
        confirm("uninstall", what)?;
    }

    remove_install(&host, route)?;

    // After the removal, so a failed one leaves the apps alone; this process
    // keeps running once its own binary is unlinked.
    let layout = Layout::user()?;
    if purge {
        purge_data(&layout)?;
    } else {
        println!(
            "kept installed apps and unpakt's data: {}, {}, {} (remove with --purge)",
            layout.data.display(),
            layout.config.display(),
            layout.cache.display()
        );
    }
    Ok(())
}

fn remove_install(host: &Host, route: Route) -> Result<()> {
    match route {
        Route::Pacman => {
            run_inherited(privileged("pacman").args(["-Rns", "unpakt"]))?;
            drop_pacman_repo()?;
        }
        Route::Deb => run_inherited(privileged("apt-get").args(["purge", "-y", "unpakt"]))?,
        Route::AppImage {
            path,
            managed: Some(scope),
        } => {
            let bin = host.bin_dir(scope);
            let real = fs::canonicalize(&path).ok();
            let mut files = vec![path.clone()];
            for link in ["unpakt", "unpakt-gui"].map(|n| bin.join(n)) {
                // Canonical, so a symlinked home still matches.
                let points_here = fs::read_link(&link).is_ok_and(|t| t == path)
                    || real.is_some() && fs::canonicalize(&link).ok() == real;
                if fs::symlink_metadata(&link).is_ok_and(|m| m.is_symlink()) && points_here {
                    files.push(link);
                }
            }
            files.extend(host.launcher_files(scope));
            files.extend(host.completion_files(scope));
            remove_paths(&files, scope)?;
            if let Some(dir) = path.parent() {
                let _ = fs::remove_dir(dir);
            }
        }
        Route::AppImage {
            path,
            managed: None,
        } => {
            println!(
                "unpakt runs from {}, which was not placed by the installer; delete that file yourself",
                path.display()
            );
        }
        Route::Binary(scope) => {
            let mut files = vec![host.exe.clone()];
            files.extend(host.completion_files(scope));
            remove_paths(&files, scope)?;
        }
        Route::Source { prefix } => {
            let tmp = tempfile::tempdir()?;
            let script = tmp.path().join("install.sh");
            fs::copy(prefix.join("lib/unpakt/install.sh"), &script)?;
            run_inherited(
                Command::new("bash")
                    .arg(&script)
                    .arg("--uninstall")
                    .env("PREFIX", &prefix),
            )?;
        }
        Route::Cargo => {
            println!("unpakt was installed with cargo; remove it with:");
            println!("  cargo uninstall unpakt-cli");
        }
        Route::Unknown => unreachable!("refused above"),
    }
    Ok(())
}

/// Removes every installed app through the engine, so launchers and commands
/// go too, then unpakt's own directories. Apps are removed one by one, so those
/// before a failing one are already gone; the directories are only deleted once
/// every app is.
fn purge_data(layout: &Layout) -> Result<()> {
    let mut engine = Engine::open_default()?;
    for app in engine.list()? {
        engine
            .remove(&app.id)
            .with_context(|| format!("cannot remove {}", app.id))?;
        println!("removed {}", app.id);
    }
    drop(engine);
    for dir in [&layout.data, &layout.config, &layout.cache] {
        match fs::remove_dir_all(dir) {
            Ok(()) => println!("removed {}", dir.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot remove {}", dir.display())),
        }
    }
    Ok(())
}

/// Takes the `[unpakt]` repository out of pacman.conf, where get.sh added it.
fn drop_pacman_repo() -> Result<()> {
    let text = match fs::read_to_string(PACMAN_CONF) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {PACMAN_CONF}")),
    };
    let cleaned = without_pacman_repo(&text);
    if cleaned == text {
        return Ok(());
    }
    let tmp = tempfile::NamedTempFile::new()?;
    fs::write(tmp.path(), cleaned)?;
    // Staged beside the target and moved over it, so pacman.conf is never half-written.
    let staged = format!("{PACMAN_CONF}.unpakt-new");
    run_inherited(
        privileged("install")
            .arg("-m644")
            .arg(tmp.path())
            .arg(&staged),
    )?;
    run_inherited(privileged("mv").arg("-f").arg(&staged).arg(PACMAN_CONF))?;
    println!("removed the [unpakt] repository from {PACMAN_CONF}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(root: &Path, exe: &str) -> Host {
        Host {
            exe: PathBuf::from(exe),
            appimage: None,
            home: root.to_path_buf(),
            cargo_home: root.join(".cargo"),
            xdg_data: root.join(".local/share"),
            xdg_config: root.join(".config"),
        }
    }

    fn nobody(_: &Path) -> Option<Package> {
        None
    }

    #[test]
    fn appimage_in_the_installer_layout_is_managed() {
        let root = Path::new("/home/u");
        let mut h = host(root, "/tmp/.mount_x/usr/bin/unpakt");
        h.appimage = Some(root.join(".local/lib/unpakt/unpakt.AppImage"));
        assert_eq!(
            detect(&h, &nobody),
            Route::AppImage {
                path: root.join(".local/lib/unpakt/unpakt.AppImage"),
                managed: Some(Scope::User)
            }
        );
        h.appimage = Some("/usr/local/lib/unpakt/unpakt.AppImage".into());
        assert!(matches!(
            detect(&h, &nobody),
            Route::AppImage {
                managed: Some(Scope::System),
                ..
            }
        ));
        h.appimage = Some("/opt/unpakt.AppImage".into());
        assert!(matches!(
            detect(&h, &nobody),
            Route::AppImage { managed: None, .. }
        ));
    }

    #[test]
    fn stale_appimage_variable_is_ignored() {
        let image = Some(PathBuf::from("/opt/x.AppImage"));
        let inside = Path::new("/tmp/.mount_x/usr/bin/unpakt");
        let dir = Some(PathBuf::from("/tmp/.mount_x"));
        assert_eq!(appimage_in_use(image.clone(), dir.clone(), inside), image);
        assert_eq!(appimage_in_use(image.clone(), None, inside), None);
        assert_eq!(
            appimage_in_use(image, dir, Path::new("/usr/bin/unpakt")),
            None
        );
        assert_eq!(
            appimage_in_use(None, Some("/tmp/.mount_x".into()), inside),
            None
        );
    }

    #[test]
    fn completion_files_map_to_their_shell() {
        let shell = |p: &str| completion_shell(Path::new(p));
        assert_eq!(
            shell("/h/.local/share/bash-completion/completions/unpakt"),
            Some("bash")
        );
        assert_eq!(
            shell("/usr/local/share/zsh/site-functions/_unpakt"),
            Some("zsh")
        );
        assert_eq!(
            shell("/h/.config/fish/completions/unpakt.fish"),
            Some("fish")
        );
        assert_eq!(shell("/h/other"), None);
    }

    #[test]
    fn appimage_wins_over_package_ownership() {
        let mut h = host(Path::new("/home/u"), "/usr/bin/unpakt");
        h.appimage = Some("/opt/x.AppImage".into());
        assert!(matches!(
            detect(&h, &|_| Some(Package::Pacman)),
            Route::AppImage { .. }
        ));
    }

    #[test]
    fn packages_are_told_apart() {
        let h = host(Path::new("/home/u"), "/usr/bin/unpakt");
        assert_eq!(detect(&h, &|_| Some(Package::Pacman)), Route::Pacman);
        assert_eq!(detect(&h, &|_| Some(Package::Deb)), Route::Deb);
    }

    #[test]
    fn source_install_needs_the_kept_installer() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin/unpakt");
        let h = host(Path::new("/nonexistent"), exe.to_str().unwrap());
        assert_eq!(detect(&h, &nobody), Route::Unknown);
        fs::create_dir_all(dir.path().join("lib/unpakt")).unwrap();
        fs::write(dir.path().join("lib/unpakt/install.sh"), "").unwrap();
        assert_eq!(
            detect(&h, &nobody),
            Route::Source {
                prefix: dir.path().to_path_buf()
            }
        );
    }

    #[test]
    fn cargo_bin_is_the_cargo_route() {
        let h = host(Path::new("/home/u"), "/home/u/.cargo/bin/unpakt");
        assert_eq!(detect(&h, &nobody), Route::Cargo);
    }

    #[test]
    fn binary_route_is_only_the_installer_locations() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("unpakt");
        fs::write(&exe, "").unwrap();
        let h = host(dir.path(), exe.to_str().unwrap());
        assert_eq!(detect(&h, &nobody), Route::Binary(Scope::User));
        let elsewhere = host(dir.path(), "/opt/bin/unpakt");
        assert_eq!(detect(&elsewhere, &nobody), Route::Unknown);
    }

    #[test]
    fn sums_give_the_latest_version() {
        let text = "\
aa11  unpakt-0.2.0-x86_64.AppImage
bb22  unpakt-0.2.0-x86_64-linux-musl
cc33 *unpakt_0.2.0-1_amd64.deb
";
        let sums = parse_sums(text);
        assert_eq!(sums.len(), 3);
        assert_eq!(sums[2].file, "unpakt_0.2.0-1_amd64.deb");
        assert_eq!(latest_version(&sums), Some(Version::new(0, 2, 0)));
        assert_eq!(latest_version(&parse_sums("aa  something-else\n")), None);
        assert_eq!(
            latest_version(&parse_sums("aa  unpakt-nope-x86_64-linux-musl\n")),
            None
        );
    }

    #[test]
    fn only_a_newer_release_updates() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(is_newer(&v("0.2.0"), &v("0.1.9")));
        assert!(!is_newer(&v("0.1.0"), &v("0.1.0")));
        assert!(!is_newer(&v("0.1.0"), &v("0.2.0")));
        assert!(!is_newer(&v("1.0.0-rc.1"), &v("1.0.0")));
    }

    #[test]
    fn pacman_section_in_the_middle_is_dropped() {
        let text = "[core]\nInclude = a\n\n[unpakt]\nSigLevel = Optional\nServer = https://x\n\n[extra]\nInclude = b\n";
        assert_eq!(
            without_pacman_repo(text),
            "[core]\nInclude = a\n\n\n[extra]\nInclude = b\n"
        );
    }

    #[test]
    fn pacman_section_at_the_end_is_dropped() {
        let text = "[core]\nInclude = a\n\n[unpakt]\nSigLevel = Optional\nServer = https://x\n";
        assert_eq!(without_pacman_repo(text), "[core]\nInclude = a\n\n");
        assert_eq!(without_pacman_repo("[unpakt]\nServer = x"), "");
    }

    #[test]
    fn pacman_section_followed_directly_by_another_is_dropped() {
        let text = "[unpakt]\nServer = x\n[extra]\nInclude = b\n";
        assert_eq!(without_pacman_repo(text), "[extra]\nInclude = b\n");
    }

    #[test]
    fn pacman_conf_without_the_section_is_unchanged() {
        let text = "[core]\nInclude = a\n# [unpakt]\n\n[extra]\nInclude = b\n";
        assert_eq!(without_pacman_repo(text), text);
        assert_eq!(without_pacman_repo(""), "");
    }

    fn gz(build: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>)) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        build(&mut b);
        b.into_inner().unwrap().finish().unwrap()
    }

    fn add_file(b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>, name: &str) {
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        // Written by hand: the builder refuses `..` and absolute names.
        h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        h.set_cksum();
        b.append(&h, &b"x"[..]).unwrap();
    }

    fn unpack(bytes: &[u8]) -> (tempfile::TempDir, Result<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar.gz");
        fs::write(&archive, bytes).unwrap();
        let out = dir.path().join("out");
        fs::create_dir(&out).unwrap();
        let result = unpack_source(&archive, &out);
        (dir, result)
    }

    #[test]
    fn source_archive_unpacks_to_its_top_directory() {
        let bytes = gz(|b| add_file(b, "unpakt-1.0.0/install.sh"));
        let (_dir, tree) = unpack(&bytes);
        let tree = tree.unwrap();
        assert!(tree.ends_with("unpakt-1.0.0"));
        assert!(tree.join("install.sh").is_file());
    }

    #[test]
    fn source_archive_rejects_escaping_paths() {
        for name in ["../evil", "unpakt-1.0.0/../../evil", "/etc/evil"] {
            let bytes = gz(|b| add_file(b, name));
            let (dir, result) = unpack(&bytes);
            assert!(result.is_err(), "{name} was accepted");
            assert!(!dir.path().join("evil").exists());
        }
    }

    #[test]
    fn source_archive_rejects_links() {
        let bytes = gz(|b| {
            let mut h = tar::Header::new_gnu();
            h.set_size(0);
            h.set_entry_type(tar::EntryType::Symlink);
            b.append_link(&mut h, "unpakt-1.0.0/link", "/etc/passwd")
                .unwrap();
        });
        let (_dir, result) = unpack(&bytes);
        assert!(result.is_err());
    }
}
