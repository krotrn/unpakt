# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.0] - 2026-10-25

### Added

- Install `.deb`, `.rpm` and pacman packages for your user, from a file, a URL
  or a GitHub release. The package is unpacked into your home directory; its
  install scripts are never run.
- A compatibility check for packages: `inspect`, `add` and `update` report
  `ok` (expected to run), `risky` or `needs-container`. A risky package is
  refused with exit status 2 unless you pass `--allow-risky`; the reasons you
  accepted are remembered per app. `needs-container` cannot be overridden.
- `unpakt add --format appimage|tarball|deb|rpm|pacman` limits the release
  assets considered, and `--allow-risky` on `add` and `update`.
- The desktop app shows the verdict in the install review and asks for
  confirmation before a risky install, and offers Update anyway when an update
  brings a new risk.
- File managers offer unpakt for `.deb`, `.rpm` and pacman packages (package
  MIME types in the launcher entry).

### Changed

- `update` keeps the format an app was installed in, and stops when a new
  version has a risk reason you have not accepted.
- Inspecting large packages is several times faster: xz payloads are decoded
  on several threads. Cancel now stops an inspect that is in progress.
- Desktop entries no longer keep a `Path=` working folder from the original
  layout, which could stop an app from starting.

## [0.1.3] - 2026-10-24

### Added

- File managers now offer unpakt for AppImages and tarballs (`.tar.gz`,
  `.tar.xz`, `.tar.zst`, `.tar.bz2`, `.tar`); opening one starts the install
  review in the desktop app.

## [0.1.2] - 2026-10-24

### Fixed

- `unpakt self uninstall` and `self update` on Arch no longer stop at
  pacman's own question after you have already confirmed (or passed `--yes`).
- The installers' closing hint no longer suggests `unpakt add owner/repo`,
  which isn't supported; use the repository's GitHub URL.

## [0.1.1] - 2026-10-24

### Added

- `unpakt self update` and `unpakt self uninstall` update or remove unpakt
  itself the way it was installed: the pacman repository, the `.deb`, the
  AppImage or static binary from `get.sh`, or `install.sh`. `self update
  --check` only checks; `self uninstall --purge` also removes the installed
  apps and unpakt's data.
- `unpakt completion bash|zsh|fish|elvish|powershell` prints a completion
  script generated from the command definitions. The Arch and Debian/Ubuntu
  packages install the bash, zsh and fish ones; `get.sh` installs bash and fish
  completions for your user.
- `get.sh`, a one-command installer that picks the pacman repository, the
  release `.deb`, the AppImage or the command-line binary for your system.
- `install.sh` builds unpakt from source and installs it into `/usr/local`
  (or any `PREFIX`); `install.sh --uninstall` removes it again. `get.sh
  --source` installs the build dependencies and runs it for the latest release.

## [0.1.0] - 2026-10-24

### Added

- Command line: `add`, `inspect`, `list`, `info`, `update`, `rollback`,
  `remove` and `doctor`. Apps install from a file, a download URL or a GitHub
  repository.
- GUI install flow with a review step, progress and cancel.
- App list with update, rollback and remove.
- A cross-process lock and crash recovery, so an interrupted install or update
  is undone.
- Desktop integration: launcher entries and a command on `PATH` for installed
  apps.

### Known limitations

- unpakt does not verify checksums or signatures of the apps it installs yet.
- unpakt does not check for updates in the background yet.
- x86_64 only.
