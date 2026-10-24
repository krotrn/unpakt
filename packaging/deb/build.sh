#!/usr/bin/env bash
# Build the Debian package into packaging/deb/out/unpakt_<version>-1_amd64.deb.
# Used by .github/workflows/packages.yml; run it from the repository root on
# Ubuntu 24.04 (the GTK 4.14 / libadwaita 1.5 floor) with a Rust toolchain on PATH.
#
#   packaging/deb/build.sh
#
# Needs: build-essential (dpkg-dev), libgtk-4-dev, libadwaita-1-dev, gzip.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out="$here/out"
cd "$root"

version=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml)
if [[ -z $version ]]; then
    echo "Could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

# Reproducible timestamps: dpkg-deb clamps file mtimes to this.
if [[ -z ${SOURCE_DATE_EPOCH:-} ]]; then
    SOURCE_DATE_EPOCH=$(git log -1 --format=%ct 2>/dev/null || date +%s)
fi
export SOURCE_DATE_EPOCH

cargo build --release --locked -p unpakt-cli -p unpakt-gui

rm -rf "$out"
stage="$out/stage"
mkdir -p "$stage/DEBIAN"

install -Dm755 -s target/release/unpakt target/release/unpakt-gui -t "$stage/usr/bin/"
install -Dm644 data/io.github.krotrn.Unpakt.desktop -t "$stage/usr/share/applications/"
install -Dm644 data/io.github.krotrn.Unpakt.metainfo.xml -t "$stage/usr/share/metainfo/"
install -Dm644 data/icons/hicolor/scalable/apps/io.github.krotrn.Unpakt.svg \
    -t "$stage/usr/share/icons/hicolor/scalable/apps/"
doc="$stage/usr/share/doc/unpakt"
install -Dm644 "$here/copyright" "$doc/copyright"
gzip -9n -c CHANGELOG.md > "$doc/changelog.gz"
chmod 644 "$doc/changelog.gz"
# Debian-format changelog (policy requires one); the upstream history is changelog.gz.
{
    printf 'unpakt (%s-1) unstable; urgency=medium\n\n' "$version"
    printf '  * Release %s. See changelog.gz for the upstream history.\n\n' "$version"
    printf ' -- krotrn <krotrn.ks@gmail.com>  %s\n' "$(date -u -d "@$SOURCE_DATE_EPOCH" '+%a, %d %b %Y %H:%M:%S +0000')"
} | gzip -9n > "$doc/changelog.Debian.gz"
chmod 644 "$doc/changelog.Debian.gz"

# Shared-library dependencies; dpkg-shlibdeps wants a debian/control in the cwd.
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/debian"
printf 'Source: unpakt\n\nPackage: unpakt\nArchitecture: amd64\n' > "$scratch/debian/control"
depends=$(cd "$scratch" && dpkg-shlibdeps -O \
    -e "$stage/usr/bin/unpakt" -e "$stage/usr/bin/unpakt-gui" | sed -n 's/^shlibs:Depends=//p')
if [[ -z $depends ]]; then
    echo "dpkg-shlibdeps produced no dependencies" >&2
    exit 1
fi

installed_size=$(du -sk "$stage" | cut -f1)

sed -e "s|@VERSION@|$version|" \
    -e "s|@INSTALLED_SIZE@|$installed_size|" \
    -e "s|@DEPENDS@|$depends|" \
    "$here/control.in" > "$stage/DEBIAN/control"

dpkg-deb --root-owner-group --build "$stage" "$out/unpakt_${version}-1_amd64.deb"
rm -rf "$stage"
ls -1 "$out"/*.deb
