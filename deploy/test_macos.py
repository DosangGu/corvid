#!/usr/bin/env python3
"""Check the DMG, nested apps, Mach-O payloads, and a real Desktop window."""
import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import time

from package import MACOS, RUNTIME_TARGETS, WINDOWS_TARGETS, digest


def run(*arguments):
    return subprocess.check_output([str(arg) for arg in arguments], text=True, timeout=60)


@contextmanager
def running(arguments, environment, log):
    process = subprocess.Popen(arguments, stdout=log, stderr=log, env=environment)
    try:
        yield process
    finally:
        process.terminate()
        process.wait(timeout=15)


def check_window(binaries, directory, environment, log):
    runtime = directory / "r"
    # Own the daemon explicitly so unmounting cannot leave a running DMG executable.
    with running([binaries / "corvid", "--runtime-dir", runtime, "daemon",
                  "--state-dir", directory / "daemon"], environment, log) as daemon:
        for _ in range(80):
            ping = subprocess.run([binaries / "corvid", "--runtime-dir", runtime, "control", "ping"],
                                  capture_output=True, timeout=10, env=environment)
            if ping.returncode == 0:
                break
            if daemon.poll() is not None:
                log.seek(0)
                raise AssertionError(log.read())
            time.sleep(0.1)
        else:
            raise AssertionError("Packaged daemon did not answer ping")
        with running([binaries / "corvid-desktop", "--state-dir", directory / "state"],
                     environment, log) as desktop:
            time.sleep(5)
            log.seek(0)
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
            for target in RUNTIME_TARGETS:
                runtime = app / "Contents/Resources/daemon" / target
                metadata = json.loads((runtime / "manifest.json").read_text())
                binary = runtime / ("corvid.exe" if target in WINDOWS_TARGETS else "corvid")
                assert metadata["sha256"] == digest(binary)
            for binary in [binaries / "corvid", binaries / "corvid-desktop",
                           app / "Contents/Helpers/Corvid Agent Deck.app/Contents/MacOS/corvid-agent-deck"]:
                assert run("lipo", "-archs", binary).strip() == "arm64"
                run(binary, "--help")
            environment = dict(os.environ, HOME=str(directory), CORVID_RUNTIME_DIR=str(directory / "r"),
                               CORVID_VIEW_STATE_DIR=str(directory / "views"))
            with (directory / "desktop.log").open("w+") as log:
                check_window(binaries, directory, environment, log)
        finally:
            run("hdiutil", "detach", mount)
    print("PASS macOS: DMG, signatures, architecture, bundled runtimes, Desktop window")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True)
    check(parser.parse_args().assets.resolve())
