import contextlib
import io
import re
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts"))

import bump_version as bump  # noqa: E402
import release_check as check  # noqa: E402
import release_notes as notes  # noqa: E402

FIXTURES = {
    "Cargo.toml": '[workspace.package]\nversion = "0.1.0"\nedition = "2021"\n',
    "crates/unpakt-cli/Cargo.toml": (
        '[dependencies]\nunpakt-core = { version = "0.1.0", path = "../unpakt-core" }\n'
    ),
    "crates/unpakt-gui/Cargo.toml": (
        '[dependencies]\nunpakt-core = { version = "0.1.0", path = "../unpakt-core" }\n'
        'serde = "1"\n'
    ),
    "Cargo.lock": "".join(
        f'[[package]]\nname = "{name}"\nversion = "0.1.0"\n\n'
        for name in ("unpakt-cli", "unpakt-core", "unpakt-gui")
    ),
    "CHANGELOG.md": "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- x\n",
    "data/io.github.krotrn.Unpakt.metainfo.xml": (
        '<?xml version="1.0" encoding="UTF-8"?>\n<component type="desktop-application">\n'
        '  <id>io.github.krotrn.Unpakt</id>\n  <releases>\n'
        '    <release version="0.1.0" date="2026-10-24"/>\n  </releases>\n</component>\n'
    ),
    "packaging/arch/PKGBUILD": "pkgver=0.1.0\npkgrel=3\n",
}
METAINFO = "data/io.github.krotrn.Unpakt.metainfo.xml"


class ReleaseScripts(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        for rel, content in FIXTURES.items():
            dest = self.root / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(content, encoding="utf-8")

    def bump(self, version, root):
        with contextlib.redirect_stdout(io.StringIO()):
            return bump.bump_version(version, root)

    def read(self, rel):
        return (self.root / rel).read_text(encoding="utf-8")

    def run_check(self, expected):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = check.main(expected, self.root)
        return code, err.getvalue()

    def snapshot(self):
        return {p.relative_to(self.root): p.read_bytes() for p in self.root.rglob("*") if p.is_file()}

    def test_check_passes_after_bump(self):
        self.assertEqual(self.bump("0.1.0", self.root), 0)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.run_check("0.1.0")[0], 0)

    def test_bump_same_version_only_releases_changelog(self):
        cargo, lock = (self.root / "Cargo.toml").read_bytes(), (self.root / "Cargo.lock").read_bytes()
        self.assertEqual(self.bump("0.1.0", self.root), 0)
        self.assertEqual((self.root / "Cargo.toml").read_bytes(), cargo)
        self.assertEqual((self.root / "Cargo.lock").read_bytes(), lock)
        today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        self.assertIn("## [Unreleased]\n\n## [0.1.0] - " + today, self.read("CHANGELOG.md"))
        self.assertEqual(self.read(METAINFO).count('version="0.1.0"'), 1)
        self.assertIn("pkgrel=1", self.read("packaging/arch/PKGBUILD"))

    def test_bump_refreshes_existing_release_date(self):
        path = self.root / METAINFO
        path.write_text(self.read(METAINFO).replace('date="2026-10-24"', 'date="2020-01-01"'), encoding="utf-8")
        self.assertEqual(self.bump("0.1.0", self.root), 0)
        today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        meta = self.read(METAINFO)
        self.assertIn(f'<release version="0.1.0" date="{today}"/>', meta)
        self.assertEqual(meta.count("<release "), 1)

    def test_bump_new_version_updates_every_source(self):
        self.assertEqual(self.bump("0.2.0", self.root), 0)
        self.assertEqual(set(check.versions(self.root).values()), {"0.2.0"})
        meta = self.read(METAINFO)
        self.assertLess(meta.index('version="0.2.0"'), meta.index('version="0.1.0"'))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(self.run_check("0.2.0")[0], 0)

    def test_check_reports_each_mismatch(self):
        lock = self.root / "Cargo.lock"
        lock.write_text(
            re.sub(r'(name = "unpakt-gui"\nversion = ")[^"]+', r"\g<1>0.0.9", lock.read_text(encoding="utf-8")),
            encoding="utf-8",
        )
        code, err = self.run_check("0.1.0")
        self.assertEqual(code, 1)
        self.assertIn("Cargo.lock", err)
        self.assertIn("'0.0.9'", err)

    def test_check_reports_missing_file(self):
        (self.root / "packaging/arch/PKGBUILD").unlink()
        code, err = self.run_check("0.1.0")
        self.assertEqual(code, 1)
        self.assertIn("PKGBUILD has None", err)

    def test_check_fails_without_changelog_section(self):
        (self.root / "CHANGELOG.md").write_text("## [Unreleased]\n", encoding="utf-8")
        code, err = self.run_check("0.1.0")
        self.assertEqual(code, 1)
        self.assertIn("no section for 0.1.0", err)

    def test_bump_rejects_bad_version(self):
        before = self.snapshot()
        for bad in ("v0.2", "0.2.0\n", "0.2.0..", "0.2.0-", "0.2.0-rc.1", "0.2"):
            self.assertEqual(self.bump(bad, self.root), 2, bad)
        self.assertEqual(self.snapshot(), before)

    def test_bump_is_all_or_nothing(self):
        gui = self.root / "crates/unpakt-gui/Cargo.toml"
        gui.write_text(
            "\n".join(l for l in gui.read_text(encoding="utf-8").splitlines() if "unpakt-core = { version" not in l),
            encoding="utf-8",
        )
        before = self.snapshot()
        self.assertEqual(self.bump("0.2.0", self.root), 3)
        self.assertEqual(self.snapshot(), before)

    def test_notes_returns_section_body(self):
        text = "## [0.1.0] - d\n\n### Added\n- x\n\n## [0.0.1]\n- y"
        self.assertEqual(notes.section("0.1.0", text), "### Added\n- x\n")


if __name__ == "__main__":
    unittest.main()
