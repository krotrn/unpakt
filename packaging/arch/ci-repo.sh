#!/usr/bin/env bash
# Turn a directory of built packages into a pacman repository named "unpakt".
# Used by .github/workflows/release.yml before uploading to the arch-repo release.
#
#   ci-repo.sh <dir>
#
# If GPG_KEY_ID is set, packages and the database are signed with that key
# and its public key is exported as unpakt.asc for users to import.
set -euo pipefail

cd "${1:?usage: ci-repo.sh <dir>}"
shopt -s nullglob
pkgs=(*.pkg.tar.zst)
if (( ${#pkgs[@]} == 0 )); then
    echo "no packages in $PWD" >&2
    exit 1
fi

sign=()
if [[ -n ${GPG_KEY_ID:-} ]]; then
    for pkg in "${pkgs[@]}"; do
        gpg --batch --yes --detach-sign --no-armor --local-user "$GPG_KEY_ID" "$pkg"
    done
    sign=(--sign --key "$GPG_KEY_ID")
    gpg --export --armor "$GPG_KEY_ID" > unpakt.asc
fi

rm -f unpakt.db* unpakt.files*
repo-add "${sign[@]}" unpakt.db.tar.gz "${pkgs[@]}"

# repo-add makes unpakt.db -> unpakt.db.tar.gz symlinks; release assets must be files.
for link in unpakt.db unpakt.files unpakt.db.sig unpakt.files.sig; do
    if [[ -L $link ]]; then
        cp --remove-destination "$(readlink -f "$link")" "$link"
    fi
done
ls -1
