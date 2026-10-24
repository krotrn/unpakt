# unpakt

unpakt installs portable Linux apps (AppImages and tarballs) for your user
account, from a local file, a download URL or a GitHub repository. It gives
each app a launcher entry and, if you want, a command on your `PATH`, keeps
the previous versions so you can roll back, and updates apps when you ask it
to. It never needs root and never writes outside your home directory.

Use it from the terminal (`unpakt`) or from the GTK 4 / libadwaita app
(`unpakt-gui`).

## Features

- Install an AppImage or a tarball from a file, a URL or a GitHub repo.
- Launcher entry with the app's own icon, and a command in `~/.local/bin`.
- `inspect` shows what an install would do before anything is changed.
- `update` fetches the latest release; `rollback` goes back one version
  (the last three versions are kept).
- Installs and updates are transactional: a failure or a Ctrl-C undoes the
  change, and a crash is recovered on the next run.
- Per user only: no root, no system packages touched.
- `doctor` checks the installation for problems.
- Shell completion for bash, zsh and fish (and elvish, PowerShell).
- A GUI with the same install, update, rollback and remove actions.

![unpakt's main window](website/screenshots/main.png)

## Install

One command for any distribution (x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/krotrn/unpakt/main/get.sh | sh
```

It picks the package for your system: the pacman repository on Arch and its
derivatives, the release `.deb` on Debian 13+ / Ubuntu 24.04+ and theirs, and
the AppImage (with `unpakt` and `unpakt-gui` commands and a launcher entry, in
`~/.local`) everywhere else. Without a graphical session, or where the desktop
app can't run (glibc older than 2.39, musl), it installs only the static
command-line binary into `~/.local/bin`. Every download is checked against the
release's `SHA256SUMS`. Run it again to update. Options go after `sh -s --`,
e.g. `curl -fsSL … | sh -s -- --cli-only`: `--cli-only`, `--gui` (desktop app
even without a graphical session), `--dry-run` (show what it would run).

Or install it yourself. Every release is on the
[releases page](https://github.com/krotrn/unpakt/releases); the commands below
use the version in `v`, so set it to the release you want.

```sh
v=0.1.0
```

### Arch Linux and derivatives

```sh
sudo tee -a /etc/pacman.conf >/dev/null <<'CONF'

[unpakt]
SigLevel = Optional TrustAll
Server = https://github.com/krotrn/unpakt/releases/download/arch-repo
CONF
sudo pacman -Sy unpakt          # or, once it is on the AUR: yay -S unpakt
sudo pacman -S --needed fuse2   # optional: run AppImages without extracting them
```

`SigLevel = Optional TrustAll` is for an unsigned repository. Once the
repository is signed, import its key and use the default `SigLevel`.

### Debian / Ubuntu

Ubuntu 24.04 or newer, Debian 13 or newer.

```sh
curl -fLO "https://github.com/krotrn/unpakt/releases/download/v$v/unpakt_${v}-1_amd64.deb"
sudo apt install "./unpakt_${v}-1_amd64.deb"
```

There is no apt repository yet: to update, install the newer `.deb` the same way.

### AppImage (any distribution, GUI and command line)

```sh
curl -fLO "https://github.com/krotrn/unpakt/releases/download/v$v/unpakt-$v-x86_64.AppImage"
chmod +x "unpakt-$v-x86_64.AppImage"
./unpakt-$v-x86_64.AppImage              # the GUI
./unpakt-$v-x86_64.AppImage cli --help   # the command line
```

To get an `unpakt` command, link to the AppImage under that name:
`ln -s "$PWD/unpakt-$v-x86_64.AppImage" ~/.local/bin/unpakt`. Where FUSE
isn't available (containers, some minimal systems), add
`--appimage-extract-and-run` after the file name.

### Command line only (any distribution)

A static binary with no dependencies:

```sh
curl -fLo unpakt "https://github.com/krotrn/unpakt/releases/download/v$v/unpakt-$v-x86_64-linux-musl"
install -Dm755 unpakt ~/.local/bin/unpakt
```

Or, once the crate is published on crates.io: `cargo install unpakt-cli`.

### From source

Needs Rust 1.93 or newer and the GTK 4 (4.14+) and libadwaita (1.5+)
development packages.

```sh
git clone https://github.com/krotrn/unpakt && cd unpakt
cargo build --release --locked
install -Dm755 target/release/unpakt target/release/unpakt-gui -t ~/.local/bin/
```

**Runtime requirements:** the command line needs nothing extra. The GUI needs
GTK 4.14+ and libadwaita 1.5+ (the packages and the AppImage bring them). FUSE 2
is optional and lets unpakt run AppImages without extracting them.

## Upgrade

Upgrade the same way you installed: `sudo pacman -Syu`, the newer `.deb`, the
newer AppImage or binary. Installed apps and their records are kept.

## Shell completion

The Arch and Debian/Ubuntu packages install completions for bash, zsh and
fish, and `get.sh` installs the bash and fish ones for your user. To install
them yourself, pick your shell (`install -D` creates the folder when it doesn't
exist yet):

```sh
unpakt completion bash | install -Dm644 /dev/stdin ~/.local/share/bash-completion/completions/unpakt
unpakt completion fish | install -Dm644 /dev/stdin ~/.config/fish/completions/unpakt.fish
unpakt completion zsh  | sudo install -Dm644 /dev/stdin /usr/local/share/zsh/site-functions/_unpakt
```

Then open a new terminal. zsh caches completions; if `unpakt <Tab>` still does
nothing, run `rm -f ~/.zcompdump*` and open another one. `unpakt completion`
also knows `elvish` and `powershell`.

## Usage

Add an app from a GitHub repository. Give the full repository URL; unpakt
installs the latest release and picks the asset that fits x86_64:

```sh
unpakt add https://github.com/owner/repo
```

From a download URL, or from a file you already have:

```sh
unpakt add https://example.com/downloads/App-1.2.3-x86_64.AppImage
unpakt add ~/Downloads/App-1.2.3-x86_64.AppImage
```

`add` shows what it is about to do and asks `Install? [y/N]`. Pass `--yes` to
skip the question (needed when there is no terminal). Useful options:
`--id`, `--command` (name of the command on `PATH`), `--no-bin`, `--desktop` /
`--no-desktop`, `--entry` (the executable inside a tarball), `--pattern REGEX`
(choose a release asset by name) and `--force` (replace files unpakt does not
own).

Look first, change nothing:

```sh
unpakt inspect https://github.com/owner/repo
```

See what is installed:

```sh
unpakt list          # one line per app
unpakt info <id>     # versions and every file unpakt created for the app
```

Update, roll back and remove:

```sh
unpakt update --check    # list available updates, install nothing
unpakt update            # update every app that has an update
unpakt update <id>       # update one app
unpakt rollback <id>     # switch back to the previous version
unpakt remove <id>       # remove the app and its files
```

Apps installed from a local file have no update source, so `update` skips
them. Check the installation:

```sh
unpakt doctor
```

`-v` shows more logging, `-vv` debug output (`RUST_LOG` overrides both):

```sh
unpakt -vv add https://github.com/owner/repo
```

### Where files go

| What | Where |
| --- | --- |
| Installed apps | `~/.local/share/unpakt/apps/<id>/<version>/`, with `current` pointing at the active one |
| Database | `~/.local/share/unpakt/db.sqlite` |
| Launcher entries | `~/.local/share/applications/unpakt-<id>.desktop` |
| Icons | `~/.local/share/icons/hicolor/` |
| Commands | `~/.local/bin/<command>` |
| Downloads (temporary) | `~/.cache/unpakt/downloads/` |

Make sure `~/.local/bin` is on your `PATH`; `unpakt doctor` tells you if it
is not. Set `UNPAKT_ROOT` to a directory to use it in place of your home
directory, for example to try unpakt without touching your real setup:

```sh
UNPAKT_ROOT=/tmp/try unpakt list
```

### Updating or removing unpakt itself

`unpakt self update` updates unpakt the way it was installed: through the
package manager, as an AppImage, as the static binary, or with the installer
script. `unpakt self update --check` only checks for a newer version. `unpakt self uninstall` removes unpakt; add `--purge` to also remove
the apps it installed and unpakt's own data.

## Troubleshooting

**An AppImage does not start, or `doctor` reports `fuse2` as not found.**
AppImages normally need FUSE 2 (`libfuse.so.2`). When unpakt does not find it,
it installs the app so that it runs with `APPIMAGE_EXTRACT_AND_RUN=1`, which
extracts the AppImage on each start instead. Installing the FUSE 2 package
(`fuse2` on Arch, `libfuse2t64` on Ubuntu 24.04) makes apps start faster. To run
an AppImage by hand without FUSE, use `APPIMAGE_EXTRACT_AND_RUN=1 ./App.AppImage`
or pass `--appimage-extract-and-run`.

**The command is not found after installing.** The command goes to
`~/.local/bin`, which must be on your `PATH`. Add it in your shell profile and
open a new terminal. `unpakt doctor` shows `bin-in-path` as `[!!]` when it is
missing.

**`conflict: ... would be shadowed by ...`.** Another program of the same name
sits earlier on your `PATH`. Choose a different name with `--command`, skip the
command with `--no-bin`, or use `--force`. The GUI offers to install without the
command.

**`conflict: ... already exists and is not managed by unpakt`.** unpakt will not
overwrite files it did not create. Move the file away, or pass `--force`.

**`no release asset matches this host`.** The release has no AppImage or
tarball for x86_64 Linux (unpakt skips other systems and architectures,
checksums and signatures). The message lists the asset names it saw. If one of
them is right but scores lower than another, select it with `--pattern`:

```sh
unpakt add https://github.com/owner/repo --pattern 'x86_64.*\.AppImage$'
```

**`http error: ... status 403` from GitHub.** Unauthenticated GitHub API
requests are rate limited. Set `GITHUB_TOKEN` (or `GH_TOKEN`) to a personal
access token; unpakt then sends it to the GitHub API, and never to download
URLs.

**An install or update was interrupted (power loss, killed process).** Nothing
to do: the next command that changes anything (`add`, `update`, `rollback`,
`remove`) first undoes the half-finished change, or finishes cleaning up after
one that had committed. If a recovery journal cannot be processed safely,
the command stops with `cannot recover the unfinished operation recorded in
<file>`, and `unpakt doctor` lists the file under `journal`. Look at the file,
fix or delete it, then retry.

**`waiting for another unpakt operation to finish...`.** Only one unpakt
changes things at a time (the CLI and the GUI share one lock). The command
waits and then continues. If nothing else is running, check for a stuck
`unpakt` process. The lock is released automatically when a process dies.

**`refusing to install without confirmation; pass --yes`.** `add` and `remove`
ask on the terminal; without one they refuse unless you pass `--yes`.

**`unsupported: ... package`.** unpakt installs AppImages and tarballs only. A
`.deb`, `.rpm`, `.zip` or a bare executable is refused.

## FAQ

**Why per user, with no root?** Installing into your home directory needs no
privileges, cannot break the system's packages, and lets each user have their
own apps. Everything unpakt writes is under `~/.local` and `~/.cache`.

**Where are my apps stored?** See [Where files go](#where-files-go).
`unpakt info <id>` lists every file created for one app.

**Does unpakt verify the apps' signatures?** No. It does not check checksums
or signatures of the apps it installs yet; see
[Known limitations](#known-limitations). Install apps from sources you trust.

**Does it update apps in the background?** No. Run `unpakt update` (or
`unpakt update --check`), or use the update action in the GUI.

**How many versions are kept?** The last three versions of each app. `rollback`
switches to the next older one.

**What happens to my apps if I uninstall unpakt?** Removing the package or the
binary leaves `~/.local/share/unpakt`, the launchers and the commands in
place, so the installed apps keep starting. You just cannot update or remove
them with unpakt until you install it again. `unpakt self uninstall --purge`
removes the apps as well.

**How does `update` know there is a new version?** For a GitHub repo it
compares the latest release tag with the installed one. For a download URL it
compares the server's `ETag` or `Last-Modified` header. Apps installed from a
local file cannot be updated.

## Compared with other tools

Short and incomplete; each tool has its strengths.

| Tool | What it is | How unpakt differs |
| --- | --- | --- |
| AppImageLauncher | Integrates AppImages into the desktop when you open them | unpakt is a command-line-first installer with updates from GitHub releases and rollback; it also installs tarballs |
| Gear Lever | GUI manager for AppImages | unpakt has a CLI as well as a GUI, and installs tarballs |
| AM / appman | A large script-based catalogue of portable apps | unpakt has no catalogue: you point it at a repo, URL or file |
| eget, bin | Download a release binary from GitHub | unpakt also creates launcher entries and icons, keeps versions and can roll back |

## Contributing

Bug reports and patches are welcome. See
[CONTRIBUTING.md](https://github.com/krotrn/unpakt/blob/main/CONTRIBUTING.md)
for setup and checks, and
[ARCHITECTURE.md](https://github.com/krotrn/unpakt/blob/main/ARCHITECTURE.md)
for how the code is organised. Documentation: <https://krotrn.github.io/unpakt/>.

## Verify a download

```sh
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify <file> -R krotrn/unpakt
minisign -Vm SHA256SUMS -P <key>   # when SHA256SUMS.minisig is published
```

This verifies unpakt's own release files, not the apps unpakt installs (see
[Known limitations](#known-limitations)). The minisign public key will be
published with the release and in this README once signing is enabled; until
then a release has no `SHA256SUMS.minisig`.

## Known limitations

- unpakt does not verify checksums or signatures of the apps it installs yet.
- unpakt does not check for updates in the background yet; run `unpakt update`.
- x86_64 only.

## Release secrets (maintainers)

Each is optional; the matching release step is skipped with a notice when it is
not set.

- `AUR_SSH_PRIVATE_KEY`: publish to the AUR
- `ARCH_GPG_PRIVATE_KEY`: sign the pacman repository; the key must not have a
  passphrase
- `MINISIGN_SECRET_KEY` and optional `MINISIGN_PASSWORD`: create
  `SHA256SUMS.minisig`
- `CARGO_REGISTRY_TOKEN`, or the repository variable
  `CRATES_IO_TRUSTED_PUBLISHING=true`: publish to crates.io. The first
  publish of each crate needs `CARGO_REGISTRY_TOKEN`, because trusted
  publishing only works for crates that already exist.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
