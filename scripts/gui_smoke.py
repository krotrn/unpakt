#!/usr/bin/env python3
"""Start the desktop app on a headless X display and check its window appears.

    python3 scripts/gui_smoke.py [--screenshot FILE] [--root DIR] [--] COMMAND [ARG...]

e.g. `scripts/gui_smoke.py unpakt-gui` or, for the AppImage,
`scripts/gui_smoke.py -- ./unpakt.AppImage --appimage-extract-and-run`.

The app runs against an empty data directory (UNPAKT_ROOT; --root picks an
existing one, e.g. with apps installed by the CLI) with
G_DEBUG=fatal-criticals, so a GTK critical crashes it. Fails when the app exits
or no "unpakt" window shows up within the timeout. Needs Xvfb and xwininfo;
the screenshot needs ImageMagick's `import`. Run it under `dbus-run-session`
where there is no session bus.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

WINDOW_TITLE = "unpakt"
STARTUP_TIMEOUT = 60  # the AppImage extracts itself first
SETTLE = 3  # seconds the window must stay up


def start_display() -> tuple[subprocess.Popen, str]:
    # A per-process display number avoids clashing with a leftover server.
    display = f":{40 + os.getpid() % 50}"
    server = subprocess.Popen(
        ["Xvfb", display, "-screen", "0", "1280x800x24", "-nolisten", "tcp"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(50):
        if subprocess.run(["xwininfo", "-root", "-display", display], capture_output=True).returncode == 0:
            return server, display
        time.sleep(0.1)
    server.kill()
    sys.exit("Xvfb did not start.")


def window_shown(display: str) -> bool:
    tree = subprocess.run(
        ["xwininfo", "-root", "-tree", "-display", display], capture_output=True, text=True
    ).stdout
    return f'"{WINDOW_TITLE}"' in tree


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--screenshot", help="save a PNG of the window here")
    parser.add_argument("--root", help="UNPAKT_ROOT to run against (default: a new empty one)")
    parser.add_argument("command", nargs="+")
    args = parser.parse_args()
    for tool in ("Xvfb", "xwininfo"):
        if not shutil.which(tool):
            sys.exit(f"{tool} is not installed.")

    server, display = start_display()
    root = args.root or tempfile.mkdtemp(prefix="unpakt-gui-smoke-")
    env = dict(
        os.environ,
        DISPLAY=display,
        GDK_BACKEND="x11",
        GSK_RENDERER="cairo",  # no GPU in CI
        G_DEBUG="fatal-criticals",
        GTK_A11Y="none",  # no accessibility bus in a bare session
        NO_AT_BRIDGE="1",
        UNPAKT_ROOT=root,
    )
    env.pop("WAYLAND_DISPLAY", None)
    app = subprocess.Popen(args.command, env=env)
    try:
        deadline = time.monotonic() + STARTUP_TIMEOUT
        while not window_shown(display):
            if app.poll() is not None:
                print(f"✘ the app exited with status {app.returncode} before showing a window")
                return 1
            if time.monotonic() > deadline:
                print(f"✘ no {WINDOW_TITLE!r} window after {STARTUP_TIMEOUT}s")
                return 1
            time.sleep(0.5)
        time.sleep(SETTLE)
        if app.poll() is not None:
            print(f"✘ the app exited with status {app.returncode} after showing its window")
            return 1
        if args.screenshot:
            if shutil.which("import"):
                subprocess.run(["import", "-display", display, "-window", WINDOW_TITLE, args.screenshot], check=True)
            else:
                print("note: ImageMagick's import is not installed; no screenshot")
        print(f"✔ the {WINDOW_TITLE!r} window is up")
        return 0
    finally:
        app.terminate()
        try:
            app.wait(timeout=10)
        except subprocess.TimeoutExpired:
            app.kill()
        server.terminate()
        if not args.root:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
