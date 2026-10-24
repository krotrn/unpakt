# Contributing to unpakt

Thanks for helping. This page covers the setup, the checks, and how changes
and releases work. For how the code fits together, read
[ARCHITECTURE.md](ARCHITECTURE.md).

## Set up

You need:

- Rust 1.93 or newer (`rust-version` in `Cargo.toml`).
- The GTK 4 (4.14+) and libadwaita (1.5+) development packages, for the GUI
  crate. On Debian/Ubuntu: `libgtk-4-dev libadwaita-1-dev`. On Arch: `gtk4
  libadwaita`.

```sh
git clone https://github.com/krotrn/unpakt && cd unpakt
cargo build --workspace
```

The workspace has three crates: `unpakt-core` (the engine), `unpakt-cli` (the
`unpakt` command) and `unpakt-gui` (the `unpakt-gui` app). `unpakt-core` must
not depend on any GUI crate; CI fails if GTK, glib, relm4 or libadwaita show up
in `cargo tree -p unpakt-core`.

## Checks

```sh
make check       # fmt --check, clippy -D warnings, tests, script tests
make test        # cargo test --workspace
make lint        # clippy with warnings denied
make fmt         # format the code
make coverage    # cargo llvm-cov (needs cargo-llvm-cov)
make gui-smoke   # start target/debug/unpakt-gui under dbus-run-session
```

Run `make check` before you open a pull request. `make gui-smoke` runs
`scripts/gui_smoke.py` against `target/debug/unpakt-gui` inside
`dbus-run-session`, so build first and have a display (or a virtual one such as
`xvfb-run`) available.

The tests need no root and no network. They install into temporary
directories (through `UNPAKT_ROOT` and explicit layouts) and talk to local
fake servers, never to GitHub. The script tests under `scripts/tests` run with
`python3 -m unittest discover -s scripts/tests`.

Packaging scripts are checked with `shellcheck`, and workflows with
`actionlint`; CI runs both.

## Reporting bugs

Open an issue and pick a template from `.github/ISSUE_TEMPLATE`. Include the
output of `unpakt --version` and `unpakt doctor`, the exact command you ran,
and the output with `-vv`. If you try something that installs apps, set
`UNPAKT_ROOT` to a scratch directory so your real home stays untouched.

## Commits and pull requests

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`type(scope): summary`, in the imperative, for example
`fix(core): report a shadowed command as its own error` or
`ci: shellcheck the installer`. Common types here are `feat`, `fix`, `docs`,
`test`, `ci`, `chore`; common scopes are `core`, `cli`, `gui`, `packaging`,
`scripts`. `git log` has plenty of examples.

Keep a change focused and add a test with it. Add a line for anything a user
would notice under `## [Unreleased]` in `CHANGELOG.md`, in the matching
`### Added`, `### Changed` or `### Fixed` list.

## Releases

Maintainers run the **Version Bump** workflow (Actions tab) with the new
version. It runs `scripts/bump_version.py`, which updates the version files,
the metainfo release entry and turns `## [Unreleased]` in the changelog into
the new version's section; then it commits, tags `vX.Y.Z` and starts the
**Release** workflow. Release checks that the versions agree with the tag, runs
the full CI and the package builds (Arch, `.deb`, AppImage), then publishes the
GitHub release with `SHA256SUMS` and build provenance, the pacman repository,
and, when the secrets are set, the AUR, crates.io and a minisign signature. See
`.github/workflows` and the release secrets in the README.

## License

By contributing you agree that your work is licensed under the project's
licenses, MIT or Apache-2.0, at the user's option.
