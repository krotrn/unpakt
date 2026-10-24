# unpakt

unpakt installs portable Linux apps (AppImages and tarballs) for your user
account, from a local file, a download URL or a GitHub repository. Installed
apps get a launcher entry and, if you want, a command on your `PATH`.

The `unpakt` command line has these commands:

- `add` installs a package file, a download URL or a GitHub repo
- `inspect` shows what installing a package would do
- `list` and `info` show installed apps
- `update` updates one app or all of them (`--check` only lists updates)
- `rollback` switches an app back to its previous version
- `remove` removes an app
- `doctor` checks the installation for problems

`unpakt-gui` is a GTK 4 / libadwaita app with the same install, update,
rollback and remove actions.

## Install

Every release is on the [releases page](https://github.com/krotrn/unpakt/releases).
The commands below use the version in `v`; set it to the release you want.
x86_64 only.

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
