# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `unpakt completion bash|zsh|fish|elvish|powershell` prints a completion
  script generated from the command definitions. The Arch and Debian/Ubuntu
  packages install the bash, zsh and fish ones; `get.sh` installs bash and fish
  completions for your user.
- `get.sh`, a one-command installer that picks the pacman repository, the
  release `.deb`, the AppImage or the command-line binary for your system.

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
