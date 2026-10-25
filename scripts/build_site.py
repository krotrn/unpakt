#!/usr/bin/env python3
"""Stage the documentation website in site-src/ for `zensical build`.

The README becomes the home page; website/*.md and the root ARCHITECTURE.md,
CONTRIBUTING.md and CHANGELOG.md the other pages. Each page gets a title and
description as front matter here, so the files on GitHub stay plain markdown.

    scripts/build_site.py && zensical build
"""

import re
import shutil
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "site-src"
REPO_BLOB = "https://github.com/krotrn/unpakt/blob/main/"
ICON = ROOT / "data/icons/hicolor/scalable/apps/io.github.krotrn.Unpakt.svg"
ROOT_PAGES = ("ARCHITECTURE.md", "CONTRIBUTING.md", "CHANGELOG.md")

# page in site-src -> (title, description)
PAGES = {
    "index.md": (
        "unpakt: install AppImages, .deb, .rpm and pacman packages on any Linux, without root",
        "Install AppImages, tarballs and .deb, .rpm or pacman packages for your user on "
        "any Linux distribution, from a file, a URL or a GitHub repository. No root, no "
        "install scripts run, a compatibility check first, updates and one-step rollback.",
    ),
    "how-it-works.md": (
        "How unpakt installs, updates and rolls back apps",
        "What happens when you install an app or a .deb, .rpm or pacman package with "
        "unpakt, where its files go, and how an interrupted install is undone.",
    ),
    "ARCHITECTURE.md": (
        "unpakt architecture",
        "How unpakt is put together: the engine, transactions and recovery, the command "
        "line, the GTK app and the packaging.",
    ),
    "CONTRIBUTING.md": (
        "Contributing to unpakt",
        "Development setup, tests, and how to report bugs and send changes.",
    ),
    "CHANGELOG.md": (
        "unpakt changelog",
        "What changed in each unpakt release.",
    ),
}


def front_matter(title: str, description: str) -> str:
    return f'---\ntitle: "{title}"\ndescription: "{description}"\n---\n\n'


def site_links(text: str, from_root: bool) -> str:
    """Links to pages on the site made relative; links to other repository files
    (from the pages that live at the repository root) pointed at GitHub."""

    def to_site(match: re.Match) -> str:
        name = match.group(1).removeprefix("website/")
        return name if name in PAGES else match.group(0)

    def to_github(match: re.Match) -> str:
        target = match.group(1)
        if re.match(r"[a-z]+:|#", target) or target in PAGES or target.startswith("website/"):
            return match.group(0)
        return f"]({REPO_BLOB}{target})"

    if from_root:
        text = re.sub(r"\]\(([^)\s]+)\)", to_github, text)
    text = re.sub(rf"{re.escape(REPO_BLOB)}([\w./-]+\.md)", to_site, text)
    # Images in website/ are copied next to the pages.
    return text.replace("](website/", "](")


def main() -> int:
    shutil.rmtree(OUT, ignore_errors=True)
    shutil.copytree(ROOT / "website", OUT, ignore=shutil.ignore_patterns("theme"))
    shutil.copy(ROOT / "README.md", OUT / "index.md")
    for name in ROOT_PAGES:
        shutil.copy(ROOT / name, OUT / name)
    (OUT / "assets").mkdir(exist_ok=True)
    shutil.copy(ICON, OUT / "assets/icon.svg")
    for name, (title, description) in PAGES.items():
        page = OUT / name
        from_root = name == "index.md" or name in ROOT_PAGES
        text = site_links(page.read_text(encoding="utf-8"), from_root)
        page.write_text(front_matter(title, description) + text, encoding="utf-8")
    missing = sorted(p.name for p in OUT.glob("*.md") if p.name not in PAGES)
    if missing:
        print(f"No title/description in PAGES for: {', '.join(missing)}")
        return 1
    print(f"Staged {len(PAGES)} pages in {OUT.relative_to(ROOT)}/")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
