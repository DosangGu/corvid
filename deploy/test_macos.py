#!/usr/bin/env python3
"""Check the DMG, nested apps, Mach-O payloads, and real Desktop and Agent Deck windows."""
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
import monitor_fixture


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


def check_window(binaries, deck, directory, environment, log):
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
let windows = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) as? [[String: Any]] ?? []
let window = windows.first { ($0[kCGWindowOwnerPID as String] as? Int) == pid && ($0[kCGWindowLayer as String] as? Int) == 0 }
print(window![kCGWindowNumber as String] as! Int)
'''
            source = directory / "window.swift"
            source.write_text(query)
            window = run("swift", source, desktop.pid).strip()
            if screenshot := os.environ.get("CORVID_DESKTOP_MACOS_SCREENSHOT"):
                subprocess.run(["screencapture", "-x", "-l", window, screenshot],
                               timeout=10, check=False)
            reader, epoch = monitor_fixture.ready(binaries / "corvid", directory / "state", environment)
        monitor_fixture.assert_same(reader, epoch, environment)
        check_deck(deck, directory, environment, log)
        monitor_fixture.assert_same(reader, epoch, environment)


def check_deck(binary, directory, environment, log):
    source = directory / "panel.swift"
    source.write_text('''import AppKit
import CoreGraphics
let pid = Int(CommandLine.arguments[1])!
let windows = CGWindowListCopyWindowInfo(.optionOnScreenOnly, kCGNullWindowID) as? [[String: Any]] ?? []
for window in windows where (window[kCGWindowOwnerPID as String] as? Int) == pid {
    if (window[kCGWindowLayer as String] as? Int) == NSWindow.Level.popUpMenu.rawValue {
        let id = window[kCGWindowNumber as String] as! Int
        let bounds = CGRect(dictionaryRepresentation: window[kCGWindowBounds as String] as! CFDictionary)!
        // Quartz Y increases downwards from the primary display's top edge.
        let primaryTop = NSScreen.screens.first!.frame.maxY
        let center = CGPoint(x: bounds.midX, y: primaryTop - bounds.midY)
        let screen = NSScreen.screens.first { $0.frame.contains(center) }!
        let offset = bounds.minY - (primaryTop - screen.visibleFrame.maxY)
        print(id, offset)
    }
}
''')
    query = directory / "panel"
    run("swiftc", source, "-o", query)
    arguments = [binary, "--state-dir", directory / "state"]
    with running([*arguments, "--background"], environment, log) as deck:
        deadline = time.monotonic() + 10
        while not any(Path(environment["CORVID_RUNTIME_DIR"]).glob("d-*/companion.sock")):
            if deck.poll() is not None:
                log.seek(0)
                raise AssertionError(log.read())
            assert time.monotonic() < deadline, "Agent Deck did not start its listener"
            time.sleep(0.1)

        assert not run(query, deck.pid).strip(), "Background Deck showed a panel"
        # The second invocation sends the same activation request as Desktop.
        subprocess.run(arguments, env=environment, check=True, timeout=10)
        window = check_panel_placement(query, deck.pid)
        if screenshot := os.environ.get("CORVID_AGENT_DECK_MACOS_SCREENSHOT"):
            subprocess.run(["screencapture", "-x", "-l", window, screenshot],
                           timeout=10, check=False)

    with running(arguments, environment, log) as deck:
        check_panel_placement(query, deck.pid)


def check_panel_placement(query, pid):
    for _ in range(40):
        output = run(query, pid).strip()
        if output:
            window, offset = output.split()
            assert window.isdigit(), "Agent Deck did not show a single AppKit panel"
            assert -1 <= float(offset) <= 12, f"Agent Deck is {offset} pt below the visible area's top"
            return window
        time.sleep(0.1)
    raise AssertionError("Agent Deck did not show a panel")


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
            deck = app / "Contents/Helpers/Corvid Agent Deck.app/Contents/MacOS/corvid-agent-deck"
            for binary in [binaries / "corvid", binaries / "corvid-desktop", deck]:
                assert run("lipo", "-archs", binary).strip() == "arm64"
                run(binary, "--help")
            environment = dict(os.environ, HOME=str(directory), CORVID_RUNTIME_DIR=str(directory / "r"),
                               CORVID_VIEW_STATE_DIR=str(directory / "views"))
            with (directory / "desktop.log").open("w+") as log:
                try:
                    check_window(binaries, deck, directory, environment, log)
                finally:
                    monitor_fixture.stop(binaries / "corvid", directory / "state", environment)
        finally:
            run("hdiutil", "detach", mount)
    print("PASS macOS: DMG, signatures, architecture, bundled runtimes, Desktop window, Agent Deck background activation")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True)
    check(parser.parse_args().assets.resolve())
