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

- AppImage and static CLI binary: download from the
  [GitHub releases](https://github.com/krotrn/unpakt/releases).
- Debian/Ubuntu: install the `.deb` from the releases page.
- Arch Linux: add the repository to `/etc/pacman.conf`, then
  `pacman -Syu unpakt`:

  ```ini
  [unpakt]
  SigLevel = Optional TrustAll
  Server = https://github.com/krotrn/unpakt/releases/download/arch-repo
  ```

  `SigLevel = Optional TrustAll` is for an unsigned repository. Once the
  repository is signed, import its key and use the default `SigLevel`.
- AUR: `unpakt`.
- From source: `cargo install unpakt-cli` (command line only).

## Verify a download

```sh
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify <file> -R krotrn/unpakt
minisign -Vm SHA256SUMS -P <key>   # when SHA256SUMS.minisig is published
```

## Upgrade

Replace the binary or AppImage, or update the package. Installed apps and their
records are kept.

## Known limitations

- unpakt does not verify checksums or signatures of the apps it installs yet.
- unpakt does not check for updates in the background yet; run `unpakt update`.
- x86_64 only.

## Release secrets (maintainers)

Each is optional; the matching release step is skipped with a notice when it is
not set.

- `AUR_SSH_PRIVATE_KEY`: publish to the AUR
- `ARCH_GPG_PRIVATE_KEY`: sign the pacman repository
- `MINISIGN_SECRET_KEY` and optional `MINISIGN_PASSWORD`: create
  `SHA256SUMS.minisig`
- `CARGO_REGISTRY_TOKEN`, or the repository variable
  `CRATES_IO_TRUSTED_PUBLISHING=true`: publish to crates.io

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
