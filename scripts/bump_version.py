#!/usr/bin/env python3
"""Bump the version across project metadata files and release the changelog.

scripts/bump_version.py 0.2.0

Exit codes: 0 done, 2 invalid version, 3 a target could not be updated (nothing is written).
"""

from __future__ import annotations

import re
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from release_check import SOURCES  # noqa: E402

CHANGELOG = "CHANGELOG.md"
METAINFO = "data/io.github.krotrn.Unpakt.metainfo.xml"
PKGBUILD = "packaging/arch/PKGBUILD"


def replace_group(text: str, pattern: str, value: str) -> str | None:
    """Replace the first match's capture group, or None when there is no match."""
    match = re.search(pattern, text, re.MULTILINE)
    if not match:
        return None
    start, end = match.span(1)
    return text[:start] + value + text[end:]


def add_metainfo_release(text: str, new_version: str, today: str) -> str | None:
    """Add ``<release version="X.Y.Z" date="DATE"/>`` before the existing releases."""
    listed = re.compile(rf'(<release version="{re.escape(new_version)}"[^>]*?date=")[^"]*(")')
    if listed.search(text):
        return listed.sub(rf"\g<1>{today}\g<2>", text, count=1)  # already listed: refresh its date
    match = re.search(r"^([ \t]*)<releases>\n", text, re.MULTILINE)
    if not match:
        return None
    entry = f'{match.group(1)}  <release version="{new_version}" date="{today}"/>\n'
    return text[: match.end()] + entry + text[match.end() :]


def release_changelog(text: str, new_version: str, today: str) -> str | None:
    """Turn ``## [Unreleased]`` into itself plus ``## [X.Y.Z] - DATE``."""
    if re.search(rf"^## \[{re.escape(new_version)}\]", text, re.MULTILINE):
        return text  # already released
    if "## [Unreleased]" not in text:
        return None
    return text.replace("## [Unreleased]", f"## [Unreleased]\n\n## [{new_version}] - {today}", 1)


def bump_version(new_version: str, root: Path = ROOT) -> int:
    if not re.fullmatch(r"\d+\.\d+\.\d+", new_version):
        print(f"Invalid version: {new_version}")
        return 2

    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    original: dict[str, str] = {}
    updated: dict[str, str] = {}

    def load(path: str) -> str | None:
        if path not in updated:
            file = root / path
            if not file.is_file():
                return None
            original[path] = updated[path] = file.read_text(encoding="utf-8")
        return updated[path]

    def fail(what: str) -> int:
        print(f"Could not update {what}")
        return 3

    for label, path, pattern in SOURCES:
        if path == METAINFO:
            continue  # handled below: new releases are added, old ones kept
        text = load(path)
        new_text = None if text is None else replace_group(text, pattern, new_version)
        if new_text is None:
            return fail(label)
        updated[path] = new_text

    text = load(PKGBUILD)
    new_text = None if text is None else re.sub(r"^pkgrel=.*$", "pkgrel=1", text, count=1, flags=re.MULTILINE)
    if new_text is None or "pkgrel=1" not in new_text:
        return fail(f"{PKGBUILD} pkgrel")
    updated[PKGBUILD] = new_text

    text = load(CHANGELOG)
    new_text = None if text is None else release_changelog(text, new_version, today)
    if new_text is None:
        return fail(f"{CHANGELOG} (no '## [Unreleased]' section to release)")
    updated[CHANGELOG] = new_text

    text = load(METAINFO)
    new_text = None if text is None else add_metainfo_release(text, new_version, today)
    if new_text is None:
        return fail(f"{METAINFO} (no <releases> element)")
    updated[METAINFO] = new_text

    for path, text in updated.items():
        if text != original[path]:
            (root / path).write_text(text, encoding="utf-8")
    print(f"Updated version to {new_version}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("Usage: scripts/bump_version.py <new-version>")
        raise SystemExit(1)
    raise SystemExit(bump_version(sys.argv[1]))
