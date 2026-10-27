#!/usr/bin/env bash
# Runs the native-install end-to-end checks in a container per distro family.
#
#   scripts/pk-e2e/host.sh [ubuntu|fedora|arch ...]   (default: all three)
#
# The static musl CLI comes from UNPAKT_BIN when set. Otherwise it is built
# inside a rust:alpine container (the repo mounted read-only, the target
# directory and crate cache in named Docker volumes so rebuilds are incremental), because the
# host may have neither rustup nor a musl target, and a glibc binary built on
# a rolling distro would not run on Ubuntu 24.04.
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

if [ -n "${UNPAKT_BIN:-}" ]; then
  install -m 755 "$UNPAKT_BIN" "$work/unpakt"
else
  echo "== building static unpakt in rust:alpine"
  docker run --rm \
    -v "$repo:/src:ro" \
    -v unpakt-pk-e2e-target:/target \
    -v unpakt-pk-e2e-cargo:/usr/local/cargo/registry \
    -v "$work:/out" \
    -e CARGO_TARGET_DIR=/target \
    -w /src \
    rust:1-alpine sh -ec '
      apk add --no-cache musl-dev gcc >/dev/null
      cargo build -p unpakt-cli --release --locked
      install -m 755 /target/release/unpakt /out/unpakt'
fi

declare -A image=([ubuntu]=ubuntu:24.04 [fedora]=fedora:latest [arch]=archlinux:latest)
families=("$@")
[ ${#families[@]} -gt 0 ] || families=(ubuntu fedora arch)

status=0
for family in "${families[@]}"; do
  [ -n "${image[$family]:-}" ] || { echo "unknown family: $family" >&2; exit 64; }
  echo "== $family (${image[$family]})"
  if docker run --rm \
    -v "$repo/scripts/pk-e2e/run.sh:/run.sh:ro" \
    -v "$work/unpakt:/unpakt:ro" \
    "${image[$family]}" bash /run.sh "$family" /unpakt; then
    echo "== $family: PASS"
  else
    echo "== $family: FAIL"
    status=1
  fi
done
exit "$status"
