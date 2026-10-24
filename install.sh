#!/usr/bin/env bash
# Build unpakt from this source tree and install it, for systems without a
# package. Prefer your distribution's package (see README) when one exists.
#
#   ./install.sh                  build, then install into /usr/local (asks for sudo)
#   ./install.sh --cli-only       only the command line (no GTK development packages needed)
#   ./install.sh --uninstall      remove everything this script installed
#   PREFIX=~/.local ./install.sh  install for your user, without sudo
#
# Run it as your user: it builds with your Rust toolchain and uses sudo only to
# write into PREFIX. Needs Rust 1.93 or newer and, for the desktop app, the
# GTK 4.14+ and libadwaita 1.5+ development packages.
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${PREFIX:-/usr/local}"
BIN="$PREFIX/bin"
LIB="$PREFIX/lib/unpakt"
SHARE="$PREFIX/share"
APP_ID=io.github.krotrn.Unpakt

say() { printf '  %s  %s\n' "$1" "$2"; }
die() { say "✘" "$*" >&2; exit 1; }

SUDO=()
as_root() { "${SUDO[@]}" "$@"; }

need_write() {  # use sudo when PREFIX (or the nearest existing parent) isn't writable
    local dir=$PREFIX
    while [[ ! -e $dir ]]; do dir=$(dirname "$dir"); done
    if [[ ! -w $dir && $EUID -ne 0 ]]; then
        command -v sudo >/dev/null 2>&1 || die "$PREFIX isn't writable; run as root or set PREFIX."
        SUDO=(sudo)
    fi
}

# Everything install_unpakt() writes, so --uninstall removes exactly that.
DESKTOP_FILES=(
    "$BIN/unpakt-gui"
    "$SHARE/applications/$APP_ID.desktop"
    "$SHARE/metainfo/$APP_ID.metainfo.xml"
    "$SHARE/icons/hicolor/scalable/apps/$APP_ID.svg"
)
FILES=(
    "$BIN/unpakt"
    "${DESKTOP_FILES[@]}"
    "$SHARE/bash-completion/completions/unpakt"
    "$SHARE/zsh/site-functions/_unpakt"
    "$SHARE/fish/vendor_completions.d/unpakt.fish"
)

# So file managers offer (or stop offering) unpakt for the types it opens.
refresh_desktop_db() {
    if command -v update-desktop-database >/dev/null 2>&1 && [[ -d $SHARE/applications ]]; then
        as_root update-desktop-database -q "$SHARE/applications" || true
    fi
}

uninstall_unpakt() {
    need_write
    as_root rm -f "${FILES[@]}"
    as_root rm -rf "$LIB"
    refresh_desktop_db
    say "✔" "unpakt removed from $PREFIX (installed apps and their records are kept)"
}

install_unpakt() {
    local gui=$1
    command -v cargo >/dev/null 2>&1 || die "cargo not found: install Rust 1.93 or newer (https://rustup.rs)."
    local packages=(-p unpakt-cli)
    [[ $gui == 1 ]] && packages+=(-p unpakt-gui)
    say "→" "Building unpakt (cargo build --release)"
    (cd "$SRC" && cargo build --release --locked "${packages[@]}")
    local target="${CARGO_TARGET_DIR:-$SRC/target}/release"

    need_write
    say "→" "Installing into $PREFIX"
    as_root install -Dm755 "$target/unpakt" "$BIN/unpakt"
    if [[ $gui == 1 ]]; then
        as_root install -Dm755 "$target/unpakt-gui" "$BIN/unpakt-gui"
        sed "s|^Exec=unpakt-gui|Exec=$BIN/unpakt-gui|" "$SRC/data/$APP_ID.desktop" |
            as_root install -Dm644 /dev/stdin "$SHARE/applications/$APP_ID.desktop"
        as_root install -Dm644 "$SRC/data/$APP_ID.metainfo.xml" "$SHARE/metainfo/$APP_ID.metainfo.xml"
        as_root install -Dm644 "$SRC/data/icons/hicolor/scalable/apps/$APP_ID.svg" \
            "$SHARE/icons/hicolor/scalable/apps/$APP_ID.svg"
        if command -v gtk-update-icon-cache >/dev/null 2>&1 && [[ -f $SHARE/icons/hicolor/index.theme ]]; then
            as_root gtk-update-icon-cache -q "$SHARE/icons/hicolor" || true
        fi
    else
        # A desktop app left over from an earlier install would be out of date.
        as_root rm -f "${DESKTOP_FILES[@]}"
    fi
    refresh_desktop_db
    "$target/unpakt" completion bash | as_root install -Dm644 /dev/stdin "$SHARE/bash-completion/completions/unpakt"
    "$target/unpakt" completion zsh | as_root install -Dm644 /dev/stdin "$SHARE/zsh/site-functions/_unpakt"
    "$target/unpakt" completion fish | as_root install -Dm644 /dev/stdin "$SHARE/fish/vendor_completions.d/unpakt.fish"
    # `unpakt self uninstall` runs this copy with --uninstall.
    as_root install -Dm644 "$SRC/install.sh" "$LIB/install.sh"

    echo
    say "✔" "$("$BIN/unpakt" --version) installed in $PREFIX"
    case ":$PATH:" in
        *":$BIN:"*) ;;
        *) say "!" "$BIN isn't on your PATH yet." ;;
    esac
    say "→" "Install an app with: unpakt add <file, URL or GitHub repo URL>"
}

gui=1
action=install
for arg in "$@"; do
    case $arg in
        --cli-only) gui=0 ;;
        --uninstall) action=uninstall ;;
        -h | --help) sed -n '2,12s/^# \{0,1\}//p' "$0"; exit 0 ;;
        *) die "Unknown option: $arg (see --help)" ;;
    esac
done

if [[ $action == uninstall ]]; then
    uninstall_unpakt
else
    install_unpakt "$gui"
fi
