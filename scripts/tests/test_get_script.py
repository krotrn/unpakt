"""get.sh, the one-command installer: which route it takes on each system."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

GET = Path(__file__).resolve().parents[2] / "get.sh"


@unittest.skipUnless(shutil.which("sh"), "needs a POSIX shell")
class GetScriptTests(unittest.TestCase):
    def dry_run(self, os_release, *args, display=False, glibc="2.41"):
        with tempfile.NamedTemporaryFile("w", suffix="os-release") as f:
            f.write(os_release)
            f.flush()
            env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
            env["UNPAKT_OS_RELEASE"] = f.name
            env["UNPAKT_GLIBC"] = glibc
            env["HOME"] = "/home/u"
            env.pop("XDG_DATA_HOME", None)
            if display:
                env["WAYLAND_DISPLAY"] = "wayland-0"
            result = subprocess.run(
                ["sh", str(GET), "--dry-run", *args], env=env, capture_output=True, text=True, check=False
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        return [line[2:] for line in result.stdout.splitlines() if line.startswith("+ ")]

    def test_arch_uses_the_pacman_repository(self):
        commands = self.dry_run("ID=endeavouros\nID_LIKE=arch\n", display=True)
        self.assertEqual(commands[-1], "sudo pacman -Sy --needed --noconfirm unpakt")

    def test_debian_and_ubuntu_derivatives_use_the_release_deb(self):
        for os_release in ("ID=debian\n", 'ID=linuxmint\nID_LIKE="ubuntu debian"\n'):
            commands = self.dry_run(os_release, display=True)
            self.assertIn("download and verify unpakt_VERSION-1_amd64.deb", commands)
            self.assertEqual(commands[-1], "sudo apt-get install -y /unpakt_VERSION-1_amd64.deb")

    def test_other_distributions_get_the_appimage_for_the_user(self):
        commands = self.dry_run("ID=fedora\n", display=True)
        self.assertIn("download and verify unpakt-VERSION-x86_64.AppImage", commands)
        self.assertIn("ln -sf /home/u/.local/lib/unpakt/unpakt.AppImage /home/u/.local/bin/unpakt", commands)
        self.assertIn("write /home/u/.local/share/applications/io.github.krotrn.Unpakt.desktop", commands)
        self.assertFalse(any(c.startswith("sudo") for c in commands))

    def test_without_a_session_only_the_command_line(self):
        commands = self.dry_run("ID=arch\n")
        self.assertIn("download and verify unpakt-VERSION-x86_64-linux-musl", commands)
        self.assertEqual(commands[-1], "install -m 755 /unpakt-VERSION-x86_64-linux-musl /home/u/.local/bin/unpakt")
        self.assertFalse(any(c.startswith("sudo") for c in commands))

    def test_gui_flags_override_the_session(self):
        self.assertIn("sudo pacman -Sy --needed --noconfirm unpakt", self.dry_run("ID=arch\n", "--gui"))
        cli = self.dry_run("ID=arch\n", "--cli-only", display=True)
        self.assertNotIn("pacman", " ".join(cli))
        self.assertIn("download and verify unpakt-VERSION-x86_64-linux-musl", cli)

    def test_old_glibc_falls_back_to_the_command_line(self):
        commands = self.dry_run("ID=debian\n", display=True, glibc="2.36")
        self.assertNotIn("apt-get", " ".join(commands))
        self.assertIn("download and verify unpakt-VERSION-x86_64-linux-musl", commands)

    def test_unknown_option_fails(self):
        result = subprocess.run(["sh", str(GET), "--bogus"], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 1)
        self.assertIn("Unknown option", result.stderr)


if __name__ == "__main__":
    unittest.main()
