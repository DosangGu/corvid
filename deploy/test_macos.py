#!/usr/bin/env python3
"""Check the DMG, nested apps, Mach-O payloads, and a real Desktop window."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import time

from package import MACOS, LINUX, WINDOWS, digest


def run(*arguments):
    return subprocess.check_output([str(arg) for arg in arguments], text=True, timeout=60)


def check(assets):
    dmg, = assets.glob(f"corvid-desktop-*-{MACOS}.dmg")
    assert dmg.with_name(dmg.name + ".sha256").read_text().split() == [digest(dmg), dmg.name]
    with tempfile.TemporaryDirectory(prefix=".cv-mac-", dir=Path.home()) as directory:
        directory = Path(directory)
        mount = directory / "volume"
        run("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", mount, dmg)
        try:
            app = mount / "Corvid.app"
            assert (mount / "Applications").is_symlink()
            run("codesign", "--verify", "--deep", "--strict", app)
            info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
            assert info["LSMinimumSystemVersion"] == "15.0"
            assert info["CFBundleIdentifier"] == "org.corvid.desktop"
            binaries = app / "Contents/MacOS"
            for target in [LINUX, WINDOWS, MACOS]:
                runtime = binaries / "daemon" / target
                metadata = json.loads((runtime / "manifest.json").read_text())
                binary = runtime / ("corvid.exe" if target == WINDOWS else "corvid")
                assert metadata["sha256"] == digest(binary)
            for binary in [binaries / "corvid", binaries / "corvid-desktop",
                           app / "Contents/Helpers/Corvid Agent Deck.app/Contents/MacOS/corvid-agent-deck"]:
                assert run("lipo", "-archs", binary).strip() == "arm64"
                run(binary, "--help")
            environment = dict(os.environ, HOME=str(directory), CORVID_RUNTIME_DIR=str(directory / "r"),
                               CORVID_VIEW_STATE_DIR=str(directory / "views"))
            with (directory / "desktop.log").open("w+") as log:
                desktop = subprocess.Popen([binaries / "corvid-desktop", "--state-dir", directory / "state"],
                                           stdout=log, stderr=log, env=environment)
                try:
                    time.sleep(5)
                    assert desktop.poll() is None, log.read()
                    # Quartz window enumeration needs no Accessibility permission.
                    query = '''import CoreGraphics
let pid = Int(CommandLine.arguments[1])!
let windows = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID) as? [[String: Any]] ?? []
assert(windows.contains { ($0[kCGWindowOwnerPID as String] as? Int) == pid && ($0[kCGWindowLayer as String] as? Int) == 0 })
'''
                    source = directory / "window.swift"
                    source.write_text(query)
                    run("swift", source, desktop.pid)
                finally:
                    desktop.terminate()
                    desktop.wait(timeout=15)
        finally:
            run("hdiutil", "detach", mount)
    print("PASS macOS: DMG, signatures, architecture, bundled runtimes, Desktop window")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True)
    check(parser.parse_args().assets.resolve())
