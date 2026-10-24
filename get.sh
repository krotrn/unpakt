#!/bin/sh
# Install unpakt with one command, the best way for this system:
#
#   curl -fsSL https://raw.githubusercontent.com/krotrn/unpakt/main/get.sh | sh
#
# With a graphical session:
#   Arch and derivatives: the unpakt pacman repository.
#   Debian 13+ / Ubuntu 24.04+ and derivatives: the latest release's .deb.
#   Other distributions: the latest release's AppImage, with the unpakt and
#     unpakt-gui commands and a launcher entry, for your user.
# Without one, and where the desktop app can't run (glibc older than 2.39,
#   musl): the static command-line binary, for your user.
#
# Every download is checked against the release's SHA256SUMS. Options (after
# `sh -s --`, e.g. curl -fsSL … | sh -s -- --cli-only):
#
#   --cli-only   install only the command line
#   --gui        install the desktop app even without a graphical session
#   --dry-run    print the commands instead of running them
#
# Running it again updates unpakt.
set -eu

REPO=krotrn/unpakt
LATEST="https://github.com/$REPO/releases/latest/download"
ARCH_REPO="https://github.com/$REPO/releases/download/arch-repo"
OS_RELEASE=${UNPAKT_OS_RELEASE:-/etc/os-release}  # tests point this at fixtures
GLIBC=${UNPAKT_GLIBC:-}                            # tests set the glibc version
MIN_GLIBC=2.39                                     # what the .deb and AppImage are built against

DRY_RUN=0
GUI=auto
SUDO=
TMP=

say() { printf '  %s  %s\n' "$1" "$2"; }
die() { say "✘" "$*" >&2; exit 1; }

usage() {
    sed -n '2,21s/^# \{0,1\}//p' "$0" 2>/dev/null || echo "See https://github.com/$REPO#install"
}

run() {  # run <command...>: run it, or print it with --dry-run
    if [ "$DRY_RUN" = 1 ]; then
        echo "+ $*"
    else
        "$@" </dev/null  # stdin may be this script (curl | sh)
    fi
}

as_root() {
    # shellcheck disable=SC2086  # $SUDO is empty or one word
    run $SUDO "$@"
}

need_root() {
    [ "$(id -u)" -eq 0 ] && return
    if command -v sudo >/dev/null 2>&1 || [ "$DRY_RUN" = 1 ]; then
        SUDO=sudo
    elif command -v doas >/dev/null 2>&1; then
        SUDO=doas
    else
        die "Run as root, or install sudo."
    fi
}

fetch() {  # fetch <url> <file>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    else
        die "Neither curl nor wget is installed."
    fi
}

tmpdir() {
    if [ -z "$TMP" ]; then
        TMP=$(mktemp -d)
        chmod 755 "$TMP"  # apt reads the .deb as its own user
        trap 'rm -rf "$TMP"' EXIT
    fi
}

release_asset() {  # release_asset <awk regex> <example>: name of the latest release's matching file
    if [ "$DRY_RUN" = 1 ]; then
        echo "$2"
        return
    fi
    [ -f "$TMP/SHA256SUMS" ] || fetch "$LATEST/SHA256SUMS" "$TMP/SHA256SUMS" || return 1
    awk -v re="$1" '$2 ~ re { print $2; exit }' "$TMP/SHA256SUMS"
}

download() {  # download <asset>: fetch it into $TMP and check it against SHA256SUMS
    if [ "$DRY_RUN" = 1 ]; then
        echo "+ download and verify $1"
        return
    fi
    fetch "$LATEST/$1" "$TMP/$1" || return 1
    if ! (cd "$TMP" && grep " $1\$" SHA256SUMS | sha256sum -c - >/dev/null); then
        say "!" "$1 failed its checksum"
        return 1
    fi
}

# ── Detection ────────────────────────────────────────────────────────────────

family() {  # arch | debian | other
    for id in ${ID:-} ${ID_LIKE:-}; do
        case $id in
            arch | archarm | manjaro* | endeavouros | cachyos) echo arch; return ;;
            debian | ubuntu | raspbian) echo debian; return ;;
        esac
    done
    echo other
}

glibc_version() {
    if [ -n "$GLIBC" ]; then
        echo "$GLIBC"
    else
        getconf GNU_LIBC_VERSION 2>/dev/null | awk '{ print $2 }'
    fi
}

desktop_can_run() {  # the .deb and the AppImage need glibc 2.39 or newer
    v=$(glibc_version)
    [ -n "$v" ] || return 1
    [ "$(printf '%s\n%s\n' "$MIN_GLIBC" "$v" | sort -V | head -n1)" = "$MIN_GLIBC" ]
}

want_gui() {
    case $GUI in
        yes) return 0 ;;
        no) return 1 ;;
    esac
    [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]
}

# ── Installers ───────────────────────────────────────────────────────────────
# Each desktop route returns non-zero when it can't install, and main() then
# installs the command line instead.

install_arch() {
    need_root
    if ! grep -q '^\[unpakt\]' /etc/pacman.conf 2>/dev/null; then
        # A repository pacman can't reach would break every later `pacman -Sy`.
        if [ "$DRY_RUN" = 0 ] && ! fetch "$ARCH_REPO/unpakt.db" /dev/null; then
            say "!" "The unpakt pacman repository isn't reachable"
            return 1
        fi
        say "→" "Adding the unpakt repository to /etc/pacman.conf"
        if [ "$DRY_RUN" = 1 ]; then
            echo "+ append [unpakt] to /etc/pacman.conf"
        else
            printf '\n[unpakt]\nSigLevel = Optional TrustAll\nServer = %s\n' "$ARCH_REPO" |
                $SUDO tee -a /etc/pacman.conf >/dev/null
        fi
    fi
    as_root pacman -Sy --needed --noconfirm unpakt
}

install_deb() {  # the latest release's .deb
    need_root
    deb=$(release_asset '^unpakt_.*_amd64\.deb$' unpakt_VERSION-1_amd64.deb) || return 1
    if [ -z "$deb" ]; then
        say "!" "The latest release has no .deb"
        return 1
    fi
    download "$deb" || return 1
    as_root apt-get update
    as_root apt-get install -y "$TMP/$deb"
}

prefix() {  # where the AppImage and the command line go: per user, or /usr/local as root
    if [ "$(id -u)" -eq 0 ] && [ "$DRY_RUN" = 0 ]; then
        BIN=/usr/local/bin LIB=/usr/local/lib/unpakt SHARE=/usr/local/share
    else
        BIN=$HOME/.local/bin LIB=$HOME/.local/lib/unpakt SHARE=${XDG_DATA_HOME:-$HOME/.local/share}
    fi
}

install_appimage() {  # the latest release's AppImage, with commands and a launcher entry
    prefix
    image=$(release_asset '^unpakt-.*-x86_64\.AppImage$' unpakt-VERSION-x86_64.AppImage) || return 1
    if [ -z "$image" ]; then
        say "!" "The latest release has no AppImage"
        return 1
    fi
    download "$image" || return 1
    version=${image#unpakt-}
    version=${version%-x86_64.AppImage}
    run mkdir -p "$LIB" "$BIN" "$SHARE/applications" "$SHARE/icons/hicolor/scalable/apps"
    run install -m 755 "$TMP/$image" "$LIB/unpakt.AppImage"
    # The AppImage runs the command line when it is called as `unpakt`.
    run ln -sf "$LIB/unpakt.AppImage" "$BIN/unpakt"
    run ln -sf "$LIB/unpakt.AppImage" "$BIN/unpakt-gui"
    icon="https://raw.githubusercontent.com/$REPO/v$version/data/icons/hicolor/scalable/apps/io.github.krotrn.Unpakt.svg"
    if [ "$DRY_RUN" = 1 ]; then
        echo "+ download $icon"
        echo "+ write $SHARE/applications/io.github.krotrn.Unpakt.desktop"
        return
    fi
    fetch "$icon" "$SHARE/icons/hicolor/scalable/apps/io.github.krotrn.Unpakt.svg" ||
        say "!" "Could not download the icon; the launcher entry has none"
    if ! command -v fusermount3 >/dev/null 2>&1 && ! command -v fusermount >/dev/null 2>&1; then
        say "!" "FUSE isn't installed: install fuse3, or set APPIMAGE_EXTRACT_AND_RUN=1 to run unpakt"
    fi
    cat >"$SHARE/applications/io.github.krotrn.Unpakt.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=unpakt
Comment=Install and manage AppImages and tarballs
Exec=$BIN/unpakt-gui
Icon=io.github.krotrn.Unpakt
Categories=System;PackageManager;GTK;
Terminal=false
StartupNotify=true
EOF
}

install_cli() {  # the latest release's static command-line binary
    prefix
    bin=$(release_asset '^unpakt-.*-x86_64-linux-musl$' unpakt-VERSION-x86_64-linux-musl) ||
        die "Could not reach the latest release on GitHub."
    [ -n "$bin" ] || die "The latest release has no command-line binary."
    download "$bin" || die "Could not download $bin."
    run mkdir -p "$BIN"
    run install -m 755 "$TMP/$bin" "$BIN/unpakt"
}

# ── Main ─────────────────────────────────────────────────────────────────────

main() {
    for arg in "$@"; do
        case $arg in
            --cli-only | --no-gui) GUI=no ;;
            --gui) GUI=yes ;;
            --dry-run) DRY_RUN=1 ;;
            -h | --help) usage; exit 0 ;;
            *) die "Unknown option: $arg (see --help)" ;;
        esac
    done

    [ "$(uname -s)" = Linux ] || [ "$DRY_RUN" = 1 ] || die "unpakt runs on Linux only."
    [ "$(uname -m)" = x86_64 ] || [ "$DRY_RUN" = 1 ] || die "unpakt is built for x86_64 only."

    if [ -r "$OS_RELEASE" ]; then
        # shellcheck disable=SC1090
        . "$OS_RELEASE"
    fi
    distro=$(family)
    [ "$DRY_RUN" = 1 ] || tmpdir

    if want_gui && ! desktop_can_run; then
        say "!" "The desktop app needs glibc $MIN_GLIBC or newer; installing the command line only"
        GUI=no
    fi
    if want_gui; then gui_note="with the desktop app"; else gui_note="command line only"; fi
    say "→" "Installing unpakt for ${PRETTY_NAME:-$distro} ($gui_note)"

    if want_gui; then
        case $distro in
            arch) install_arch ;;
            debian) install_deb ;;
            *) install_appimage ;;
        esac || {
            say "→" "Could not install the desktop app; installing the command line instead"
            install_cli
        }
    else
        install_cli
    fi

    [ "$DRY_RUN" = 1 ] && return
    if ! command -v unpakt >/dev/null 2>&1; then
        say "!" "unpakt is installed in ${BIN:-/usr/bin}, which isn't on your PATH yet."
        say " " "Add it to PATH, or open a new terminal."
        return
    fi
    echo
    say "✔" "$(unpakt --version 2>/dev/null) installed."
    say "→" "Install an app with: unpakt add <file, URL or owner/repo>"
}

main "$@"
