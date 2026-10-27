#!/usr/bin/env bash
# End-to-end checks of native installs: the real static unpakt CLI against the
# real PackageKit daemon of a distro. Runs inside a container, as root (so
# polkit needs no prompt), and stops at the first failed check.
#
#   run.sh <ubuntu|fedora|arch> <path-to-unpakt>
set -euo pipefail

family=${1:?usage: run.sh <ubuntu|fedora|arch> <path-to-unpakt>}
unpakt=$(readlink -f "${2:?usage: run.sh <ubuntu|fedora|arch> <path-to-unpakt>}")
[ "$(id -u)" -eq 0 ] || { echo "run.sh must run as root" >&2; exit 64; }

step() { printf '\n--- %s\n' "$*"; }
fail() { printf '\nFAILED CHECK: %s\n' "$*" >&2; exit 1; }

# check "<description>" <command...>: runs the command, fails the run if it fails.
check() {
  local what=$1
  shift
  if "$@"; then printf 'ok   %s\n' "$what"; else fail "$what"; fi
}

# check_not "<description>" <command...>: the command must fail.
check_not() {
  local what=$1
  shift
  if "$@"; then fail "$what"; else printf 'ok   %s\n' "$what"; fi
}

# expect_status "<description>" <status> <command...>: the command's exit status must match.
expect_status() {
  local what=$1 want=$2 got=0
  shift 2
  "$@" >"$out" 2>&1 </dev/null || got=$?
  cat "$out"
  [ "$got" -eq "$want" ] || fail "$what (exit $got, wanted $want)"
  printf 'ok   %s\n' "$what"
}

out=$(mktemp)
work=$(mktemp -d)
mkdir -p "$work/serve" "$work/build"
export HOME=$work/home
mkdir -p "$HOME"
export UNPAKT_PACKAGEKIT=1

# ---------------------------------------------------------------- 1. tools
step "install PackageKit and build tools ($family)"
case $family in
  ubuntu)
    export DEBIAN_FRONTEND=noninteractive
    # unpakt builds its HTTP client from the system CA store, even for plain http URLs.
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      packagekit dbus python3 dpkg-dev binutils libarchive-tools zstd passwd ca-certificates shared-mime-info >/dev/null
    ext=deb
    ;;
  fedora)
    dnf install -y -q PackageKit polkit dbus-daemon rpm-build python3 binutils bsdtar zstd shadow-utils >/dev/null
    ext=rpm
    ;;
  arch)
    # PackageKit's post-transaction hook runs `pkcon refresh`, which fails
    # because no bus is running yet; pacman then exits non-zero although every
    # package is installed. Check the packages instead of the exit status.
    pacman -Syu --noconfirm --needed packagekit polkit dbus python binutils libarchive zstd shadow shared-mime-info >/dev/null || true
    pacman -Q packagekit polkit dbus python binutils libarchive zstd shadow shared-mime-info >/dev/null
    ext=pkg.tar.zst
    ;;
  *) echo "unknown family: $family" >&2; exit 64 ;;
esac

# ------------------------------------------------------------ 2. system bus
step "start the system bus"
mkdir -p /run/dbus
dbus-uuidgen --ensure
rm -f /run/dbus/pid
dbus-daemon --system --fork
# packagekitd is started by D-Bus activation on the first call. Without
# systemd, polkitd is started by hand where the distro asks for it: PackageKit
# checks authorization through it even for root.
if [ "$family" != ubuntu ]; then
  polkitd=$(find /usr/lib /usr/libexec -name polkitd -type f 2>/dev/null | head -1 || true)
  [ -n "$polkitd" ] || fail "polkitd not found"
  "$polkitd" --no-debug >"$work/polkitd.log" 2>&1 &
  sleep 1
fi

# ------------------------------------------------------- 3. native packages
# native_build <name> <version> <outfile>: a package with a shell entry point
# and an install script that records that it ran.
native_build() {
  local name=$1 version=$2 dest=$3 root
  root=$(mktemp -d "$work/build/XXXXXX")
  mkdir -p "$root/usr/bin"
  printf '#!/bin/sh\necho hello from %s %s\n' "$name" "$version" >"$root/usr/bin/$name"
  chmod 755 "$root/usr/bin/$name"
  case $family in
    ubuntu)
      mkdir -p "$root/DEBIAN"
      cat >"$root/DEBIAN/control" <<CTL
Package: $name
Version: $version
Architecture: all
Maintainer: pk-e2e <pk-e2e@example.invalid>
Description: end-to-end test package
CTL
      printf '#!/bin/sh\ntouch /var/tmp/pk-hello-ran\n' >"$root/DEBIAN/postinst"
      chmod 755 "$root/DEBIAN/postinst"
      dpkg-deb --root-owner-group --build "$root" "$dest" >/dev/null
      ;;
    fedora)
      mkdir -p "$work/rpm"
      cat >"$work/rpm/$name.spec" <<SPEC
Name: $name
Version: $version
Release: 1
Summary: end-to-end test package
License: MIT
BuildArch: noarch
%description
end-to-end test package
%install
mkdir -p %{buildroot}/usr/bin
cp $root/usr/bin/$name %{buildroot}/usr/bin/$name
%post
touch /var/tmp/pk-hello-ran
%files
/usr/bin/$name
SPEC
      rpmbuild -bb --quiet --define "_topdir $work/rpm" "$work/rpm/$name.spec"
      cp "$work/rpm/RPMS/noarch/$name-$version-1.noarch.rpm" "$dest"
      ;;
    arch)
      cat >"$root/.PKGINFO" <<INFO
pkgname = $name
pkgver = $version
pkgdesc = end-to-end test package
arch = any
size = 1
INFO
      printf 'post_install() {\n  touch /var/tmp/pk-hello-ran\n}\npost_upgrade() {\n  post_install\n}\n' >"$root/.INSTALL"
      (cd "$root" && bsdtar --zstd -cf "$dest" .PKGINFO .INSTALL usr)
      ;;
  esac
}

# Versions as the native tool names them: Debian and RPM files use the plain
# version, Arch needs a release suffix.
ver() { if [ "$family" = arch ]; then echo "$1-1"; else echo "$1"; fi; }

# risky_build <outfile>: a package of the other family whose install script
# creates a user; unpakt must refuse it.
risky_build() {
  local dest=$1 root
  root=$(mktemp -d "$work/build/XXXXXX")
  mkdir -p "$root/usr/bin"
  printf '#!/bin/sh\necho risky\n' >"$root/usr/bin/pk-risky"
  chmod 755 "$root/usr/bin/pk-risky"
  if [ "$family" = ubuntu ]; then
    cat >"$root/.PKGINFO" <<INFO
pkgname = pk-risky
pkgver = 1.0-1
pkgdesc = end-to-end test package
arch = any
size = 1
INFO
    printf 'post_install() {\n  useradd pk-risky\n}\n' >"$root/.INSTALL"
    (cd "$root" && bsdtar --zstd -cf "$dest" .PKGINFO .INSTALL usr)
  else
    mkdir -p "$root/DEBIAN"
    cat >"$root/DEBIAN/control" <<CTL
Package: pk-risky
Version: 1.0
Architecture: all
Maintainer: pk-e2e <pk-e2e@example.invalid>
Description: end-to-end test package
CTL
    printf '#!/bin/sh\nuseradd pk-risky\n' >"$root/DEBIAN/postinst"
    chmod 755 "$root/DEBIAN/postinst"
    # A .deb is an ar archive of three members, in this order.
    (
      cd "$root"
      printf '2.0\n' >"$work/build/debian-binary"
      tar --owner=0 --group=0 -czf "$work/build/control.tar.gz" -C DEBIAN .
      tar --owner=0 --group=0 -czf "$work/build/data.tar.gz" usr
      ar rc "$dest" "$work/build/debian-binary" "$work/build/control.tar.gz" "$work/build/data.tar.gz"
    )
  fi
}

# Native queries.
is_installed() {
  case $family in
    ubuntu) dpkg-query -W -f '${Status}' "$1" 2>/dev/null | grep -q '^install ok installed' ;;
    fedora) rpm -q "$1" >/dev/null 2>&1 ;;
    arch) pacman -Q "$1" >/dev/null 2>&1 ;;
  esac
}
installed_version() {
  case $family in
    ubuntu) dpkg-query -W -f '${Version}' "$1" ;;
    fedora) rpm -q --qf '%{VERSION}-%{RELEASE}' "$1" | sed 's/-1$//' ;;
    arch) pacman -Q "$1" | awk '{print $2}' ;;
  esac
}
native_install() {
  case $family in
    ubuntu) dpkg -i "$1" ;;
    fedora) rpm -U "$1" ;;
    arch) pacman -U --noconfirm "$1" ;;
  esac
}

# restart_packagekit: PackageKit's dnf backend keeps its own view of the
# installed set and does not notice a package changed behind its back, not even
# after a minute. Stopping the daemon makes D-Bus start a fresh one, which
# reads the real database, as a daemon that went idle would.
restart_packagekit() {
  local comm
  for comm in /proc/[0-9]*/comm; do
    if [ "$(cat "$comm" 2>/dev/null)" = packagekitd ]; then kill "$(basename "$(dirname "$comm")")"; fi
  done
  sleep 2
}

step "build the test packages"
serve=$work/serve
native_build pk-hello "$(ver 1.0)" "$serve/pk-hello.$ext"
native_build pk-other "$(ver 1.0)" "$serve/pk-other.$ext"
native_build pk-hello "$(ver 2.0)" "$work/pk-hello-2.0.$ext"
native_build pk-hello "$(ver 2.1)" "$work/pk-hello-2.1.$ext"
if [ "$family" = ubuntu ]; then risky_ext=pkg.tar.zst; else risky_ext=deb; fi
risky_build "$serve/pk-risky.$risky_ext"
ls -l "$serve"

# ------------------------------------------------------------- 4. file server
step "serve the packages on 127.0.0.1"
(cd "$serve" && exec python3 -m http.server 8765 --bind 127.0.0.1 >"$work/http.log" 2>&1) &
for _ in $(seq 50); do
  python3 -c 'import urllib.request as u; u.urlopen("http://127.0.0.1:8765/")' 2>/dev/null && break
  sleep 0.2
done
url() { echo "http://127.0.0.1:8765/$1"; }

# ----------------------------------------------------------------- 5. checks
new_root() { UNPAKT_ROOT=$(mktemp -d "$work/root-XXXXXX"); export UNPAKT_ROOT; }
u() { "$unpakt" "$@"; }

step "unpakt basics"
check "binary runs" u --version
u --help | head -30
u info --help | head -30

if [ "$family" != arch ]; then
  step "add --yes installs through PackageKit"
  new_root
  rm -f /var/tmp/pk-hello-ran
  expect_status "add --yes pk-hello 1.0 exits 0" 0 u add --yes "$(url "pk-hello.$ext")"
  check "native query shows 1.0" test "$(installed_version pk-hello)" = "$(ver 1.0)"
  check "install script ran" test -e /var/tmp/pk-hello-ran
  u info pk-hello | tee "$out"
  check "info shows backend: packagekit" grep -q 'backend: packagekit' "$out"
  home_root=$UNPAKT_ROOT

  step "add without consent is refused"
  new_root
  expect_status "add without --yes and no tty exits 2" 2 u add "$(url "pk-other.$ext")"
  check_not "pk-other is not installed" is_installed pk-other

  step "a non-root user without a polkit agent is denied"
  # Polkit asks for administrator authentication to install an untrusted local
  # package; with nobody to ask, PackageKit must report NotAuthorized and
  # unpakt must leave nothing behind.
  useradd -m pkuser
  pkroot=$(mktemp -d /tmp/pkuser-root-XXXXXX)
  chown pkuser: "$pkroot"
  expect_status "add --yes as a non-root user exits 2" 2 \
    runuser -u pkuser -- env UNPAKT_ROOT="$pkroot" UNPAKT_PACKAGEKIT=1 "$unpakt" add --yes "$(url "pk-other.$ext")"
  check "the refusal says authorization was refused" grep -q 'authorization was refused' "$out"
  check_not "pk-other is not installed" is_installed pk-other
  check "no native journal is left" test -z "$(ls -A "$pkroot/.local/share/unpakt/native" 2>/dev/null)"

  step "update keeps the newer native version"
  UNPAKT_ROOT=$home_root
  export UNPAKT_ROOT
  check "install 2.1 natively" native_install "$work/pk-hello-2.1.$ext"
  restart_packagekit
  cp "$work/pk-hello-2.0.$ext" "$serve/pk-hello.$ext"
  expect_status "update --yes exits 0" 0 u update pk-hello --yes
  check "update said up to date" grep -q 'pk-hello: up to date' "$out"
  check "native version is still 2.1" test "$(installed_version pk-hello)" = "$(ver 2.1)"
  u info pk-hello | tee "$out"
  check "info lists 2.1" grep -q '2\.1' "$out"

  step "remove --yes removes the package"
  expect_status "remove --yes exits 0" 0 u remove --yes pk-hello
  check_not "pk-hello is gone" is_installed pk-hello
  u list | tee "$out"
  check "list prints nothing" test ! -s "$out"
else
  # PackageKit's alpm backend cannot install a local pacman package (and
  # enforces SigLevel on unsigned files), so unpakt extracts it and prints the
  # command for a system-wide install.
  step "a pacman package is extracted, not installed through PackageKit"
  new_root
  expect_status "add --yes pk-hello exits 0" 0 u add --yes "$(url "pk-hello.$ext")"
  check "the output says why PackageKit was skipped" grep -q 'PackageKit cannot install local pacman packages' "$out"
  check "the output prints the pacman command" grep -q 'sudo pacman -U' "$out"
  check_not "pk-hello is not installed natively" is_installed pk-hello
  u info pk-hello | tee "$out"
  check "info shows backend: extract" grep -q 'backend: extract' "$out"
  expect_status "remove --yes exits 0" 0 u remove --yes pk-hello
  u list | tee "$out"
  check "list prints nothing" test ! -s "$out"
fi

step "a risky foreign package is refused"
new_root
expect_status "add --yes of a foreign package with a user-creating script exits 2" 2 \
  u add --yes "$(url "pk-risky.$risky_ext")"
check_not "pk-risky user does not exist" id pk-risky

printf '\nall checks passed (%s)\n' "$family"
