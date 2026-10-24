#!/usr/bin/env python3
"""Check that every place that carries the version agrees, before publishing.

scripts/release_check.py 0.1.0      # prints "version=0.1.0" on success
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from release_notes import section  # noqa: E402

# (label, relative path, regex with one capture group around the version)
SOURCES: list[tuple[str, str, str]] = [
    ("Cargo.toml", "Cargo.toml", r'^version = "([^"]+)"'),
    (
        "crates/unpakt-cli/Cargo.toml",
        "crates/unpakt-cli/Cargo.toml",
        r'unpakt-core = \{ version = "([^"]+)"',
    ),
    (
        "crates/unpakt-gui/Cargo.toml",
        "crates/unpakt-gui/Cargo.toml",
        r'unpakt-core = \{ version = "([^"]+)"',
    ),
    ("Cargo.lock (unpakt-core)", "Cargo.lock", r'^name = "unpakt-core"\nversion = "([^"]+)"'),
    ("Cargo.lock (unpakt-cli)", "Cargo.lock", r'^name = "unpakt-cli"\nversion = "([^"]+)"'),
    ("Cargo.lock (unpakt-gui)", "Cargo.lock", r'^name = "unpakt-gui"\nversion = "([^"]+)"'),
    ("packaging/arch/PKGBUILD", "packaging/arch/PKGBUILD", r"^pkgver=(\S+)"),
    (
        "data/io.github.krotrn.Unpakt.metainfo.xml",
        "data/io.github.krotrn.Unpakt.metainfo.xml",
        r'<release version="([^"]+)"',
    ),
]


def versions(root: Path = ROOT) -> dict[str, str | None]:
    """The version found in each source, keyed by label; None when missing."""
    found: dict[str, str | None] = {}
    for label, path, pattern in SOURCES:
        file = root / path
        text = file.read_text(encoding="utf-8") if file.is_file() else ""
        match = re.search(pattern, text, re.MULTILINE)
        found[label] = match.group(1) if match else None
    return found


def main(expected: str, root: Path = ROOT) -> int:
    problems = [f"{label} has {got!r}, expected {expected!r}" for label, got in versions(root).items() if got != expected]
    try:
        section(expected, (root / "CHANGELOG.md").read_text(encoding="utf-8"))
    except SystemExit as exc:
        problems.append(str(exc))
    for problem in problems:
        print(f"::error::{problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"version={expected}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    raise SystemExit(main(sys.argv[1]))
