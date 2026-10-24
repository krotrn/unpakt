#!/usr/bin/env bash
# Build the AppImage into packaging/appimage/out/unpakt-<version>-x86_64.AppImage.
# Used by .github/workflows/packages.yml; run it from the repository root on
# Ubuntu 24.04 (the GTK 4.14 / libadwaita 1.5 floor) with a Rust toolchain on PATH.
#
#   packaging/appimage/build.sh
#
# Needs (apt): build-essential dpkg-dev pkg-config curl file patchelf
#   desktop-file-utils libgtk-4-dev libadwaita-1-dev librsvg2-common
#   libglib2.0-bin shared-mime-info
# Smoke-testing the GUI additionally needs: xvfb xauth
set -euo pipefail
umask 022

# Pinned build tools (linuxdeploy 1-alpha-20251107-1; plugin-gtk commit 3b67a1d).
linuxdeploy_url=https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage
linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
gtk_plugin_url=https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/3b67a1d1c1b0c8268f57f2bce40fe2d33d409cea/linuxdeploy-plugin-gtk.sh
gtk_plugin_sha256=b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out="$here/out"
cd "$root"

version=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml)
if [[ -z $version ]]; then
    echo "Could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

cargo build --release --locked -p unpakt-cli -p unpakt-gui

rm -rf "$out"
mkdir -p "$out"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() { # url sha256 destination
    curl -fsSL --retry 3 -o "$3" "$1"
    echo "$2  $3" | sha256sum -c --quiet -
}
fetch "$linuxdeploy_url" "$linuxdeploy_sha256" "$work/linuxdeploy-x86_64.AppImage"
fetch "$gtk_plugin_url" "$gtk_plugin_sha256" "$work/linuxdeploy-plugin-gtk.sh"
chmod +x "$work/linuxdeploy-x86_64.AppImage" "$work/linuxdeploy-plugin-gtk.sh"

appdir="$work/AppDir"
id=io.github.krotrn.Unpakt
install -Dm755 -s target/release/unpakt target/release/unpakt-gui -t "$appdir/usr/bin/"
install -Dm644 "data/$id.desktop" -t "$appdir/usr/share/applications/"
install -Dm644 "data/$id.metainfo.xml" -t "$appdir/usr/share/metainfo/"
install -Dm644 "data/icons/hicolor/scalable/apps/$id.svg" \
    -t "$appdir/usr/share/icons/hicolor/scalable/apps/"

(
    cd "$work"
    PATH="$work:$PATH" DEPLOY_GTK_VERSION=4 LINUXDEPLOY_OUTPUT_VERSION="$version" \
        ./linuxdeploy-x86_64.AppImage --appimage-extract-and-run \
        --appdir AppDir --plugin gtk \
        --executable "$appdir/usr/bin/unpakt" --executable "$appdir/usr/bin/unpakt-gui" \
        --desktop-file "$appdir/usr/share/applications/$id.desktop" \
        --icon-file "$appdir/usr/share/icons/hicolor/scalable/apps/$id.svg" \
        --custom-apprun "$here/AppRun" --output appimage
)

mv "$work/unpakt-${version}-x86_64.AppImage" "$out/"
ls -1 "$out"/*.AppImage
