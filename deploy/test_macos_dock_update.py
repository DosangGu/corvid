#!/usr/bin/env python3
"""Verify published macOS installs and their original Dock references after an update."""

import argparse
import fcntl
from contextlib import contextmanager, nullcontext
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import time
import uuid
from urllib.parse import unquote, urlsplit

from macos_package import build_relaunch_helper, plist, sign
from package import digest

ROOT = Path(__file__).resolve().parents[1]
CLI = "Contents/MacOS/corvid"
BUNDLE = "org.corvid.desktop"


def run(*args, **kwargs):
    return subprocess.check_output([str(arg) for arg in args], text=True, timeout=120, **kwargs).strip()


def launch_dock_tile(dock, app, alias, tile):
    run(dock, "resolve", app, alias, tile)
    script = """
on run argv
    set appName to item 1 of argv
    tell application "System Events"
        if not (exists process "Dock") then return "waiting for Dock"
        tell process "Dock"
            if not (exists list 1) then return "waiting for Dock items"
            set entries to UI elements of list 1
            repeat with entry in entries
                if name of entry is appName then
                    perform action "AXPress" of entry
                    return "pressed"
                end if
            end repeat
            repeat with entry in entries
                if name of entry is "Corvid" then
                    perform action "AXPress" of entry
                    return "pressed"
                end if
            end repeat
        end tell
    end tell
    return "missing"
end run
"""
    for _ in range(20):
        if run("/usr/bin/osascript", "-e", script, app.stem) == "pressed":
            run(dock, "running", app, alias, tile)
            return
        time.sleep(0.3)
    raise AssertionError("Could not find Corvid's existing Dock tile")


@contextmanager
def hide_recent_dock_apps():
    # Both fixtures have Corvid's bundle name. Exclude recently launched apps
    # so AXPress only sees the pinned tiles and currently running applications.
    assert os.environ.get("GITHUB_ACTIONS") == "true"
    key = ("com.apple.dock", "show-recents")
    previous = subprocess.run(["defaults", "read", *key], capture_output=True, text=True, timeout=10)
    run("defaults", "write", *key, "-bool", "false")
    try:
        yield
    finally:
        if previous.returncode == 0:
            run("defaults", "write", *key, "-bool", "true" if previous.stdout.strip() == "1" else "false")
        else:
            run("defaults", "delete", *key)
        run("killall", "Dock")


def release_fixture(archive, directory):
    release = directory / "release"
    release.mkdir()
    version = archive.name.removeprefix("corvid-desktop-").removesuffix("-aarch64-apple-darwin.tar.gz")
    revision = run("git", "rev-parse", "HEAD")
    shutil.copyfile(ROOT / "deploy/install.sh", release / "install.sh")
    (release / archive.name).symlink_to(archive)
    files = [release / "install.sh", archive]
    (release / "release-manifest.json").write_text(json.dumps({
        "schema_version": 1, "version": version, "source_revision": revision,
        "assets": [{"name": p.name, "size": p.stat().st_size, "sha256": digest(p)} for p in files],
    }))
    (release / f"v{version}").write_text(json.dumps({
        "tag_name": f"v{version}", "draft": False, "prerelease": False,
    }))
    curl = release / "curl"
    curl.write_text(f"""#!{os.path.realpath(shutil.which('python3'))}
import pathlib, shutil, sys
root = pathlib.Path(__file__).parent
shutil.copyfile(root / sys.argv[-1].rsplit("/", 1)[1], sys.argv[sys.argv.index("--output") + 1])
""")
    curl.chmod(0o755)
    fixture_home = directory / "install-home"
    fixture_home.mkdir()
    env = dict(os.environ, HOME=str(fixture_home), PATH=f"{release}:{os.environ['PATH']}")
    return version, revision, fixture_home, env


def run_update(app, release):
    version, _, _, env = release
    return subprocess.run([app / CLI, "update", "--version", version], env=env,
                          capture_output=True, text=True, timeout=120)


def update(app, release):
    version, revision, fixture_home, _ = release
    result = run_update(app, release)
    assert result.returncode == 0, result.stdout + result.stderr
    assert not (fixture_home / "Applications/Corvid.app").exists(), result.stdout
    metadata = json.loads((app / "Contents/Resources/package.json").read_text())
    assert metadata["source_revision"] == revision and metadata["version"] == version
    assert run(app / CLI, "--version") == f"corvid {version}"
    return version


def check_managed_update(previous, archive, directory, dock, live_dock):
    # Lay out the legacy installation directly so the candidate installer always
    # exercises migration, even when the previous release uses a different installer.
    user = directory / "managed-home"
    root = user / "Library/Application Support/Corvid/install/desktop"
    binaries = user / ".local/bin"
    applications = user / "Applications"
    binaries.mkdir(parents=True)
    applications.mkdir()
    old_version = previous.name.removeprefix("corvid-desktop-").removesuffix("-aarch64-apple-darwin.tar.gz")
    old = root / "versions" / old_version
    old.mkdir(parents=True)
    with tarfile.open(previous) as package:
        package.extractall(old, filter="data")
    (root / "current").symlink_to(Path("versions") / old_version)
    (root / "install.json").write_text('{"schema_version":1,"product":"desktop"}\n')
    (root / "bin-dir").write_text(f"{binaries}\n")
    app = applications / "Corvid.app"
    app.symlink_to(root / "current/Corvid.app")
    for binary in ("corvid", "corvid-desktop"):
        (binaries / binary).symlink_to(root / "current/Corvid.app/Contents/MacOS" / binary)

    bookmark = directory / "managed-bookmark"
    run(dock, "save-bookmark", app, bookmark)
    tile_bookmark = directory / "managed-dock-tile"
    tile = run(dock, "pin", app, tile_bookmark) if live_dock else None
    try:
        original_inode = app.stat().st_ino
        release = release_fixture(archive, directory)
        version, _, _, environment = release
        environment["HOME"] = str(user)
        result = run_update(app, release)
        assert result.returncode == 0, result.stdout + result.stderr
        assert not app.is_symlink(), "Desktop must launch from a stable, real app bundle"
        assert app.stat().st_ino == original_inode, "The legacy app directory identity was lost"
        assert not old.exists(), "Unused version directories must still be retired"
        resolved = Path(run(dock, "resolve-bookmark", app, bookmark)).resolve()
        assert resolved == app, f"Original Dock bookmark points to {resolved}"
        assert run(app / CLI, "--version") == f"corvid {version}"
        assert run(binaries / "corvid", "--version") == f"corvid {version}"
        run("codesign", "--verify", "--deep", "--strict", app)
        if tile:
            launch_dock_tile(dock, app, tile_bookmark, tile)
    finally:
        if tile:
            run(dock, "remove", app, tile_bookmark, tile)
    print("PASS legacy managed Desktop bookmark survives its in-place CLI update")


def check_running_relaunch(app, directory, dock, alias, entry):
    state = directory / f"restored windows {Path(entry).name}"
    window = str(uuid.uuid4())
    view = state / "desktop-view"
    view.mkdir(parents=True, mode=0o700)
    registry = view / "windows.json"
    registry.write_text(json.dumps({"schema_version": 1, "windows": [window]}))
    registry.chmod(0o600)
    with tempfile.TemporaryDirectory(prefix="corvid-desktop-ready-") as handoff:
        run(dock, "handoff", app, alias, entry, handoff, "--state-dir", state,
            "--restore-window", window)
        run(dock, "running", app, alias)
        lease = state / "desktop-view" / "windows" / window / "startup.lock"
        for _ in range(200):
            if lease.exists():
                with lease.open("rb") as opened:
                    try:
                        fcntl.flock(opened, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        return
            time.sleep(0.05)
        raise AssertionError(f"The relaunched Desktop did not reclaim window {window}")


def check_single_dock_tile(app):
    script = """
set dockEntries to {}
tell application "System Events" to tell process "Dock"
    if exists list 1 then
        repeat with entry in UI elements of list 1
            set itemName to name of entry as text
            set itemAddress to ""
            try
                set itemAddress to value of attribute "AXURL" of entry as text
            end try
            set end of dockEntries to itemName & tab & itemAddress
        end repeat
    end if
end tell
set AppleScript's text item delimiters to linefeed
return dockEntries as text
"""
    app = app.resolve()
    deadline = time.monotonic() + 10
    previous = []
    while True:
        entries = run("/usr/bin/osascript", "-e", script).splitlines()
        matches = []
        for entry in entries:
            name, _, address = entry.partition("\t")
            url = urlsplit(address)
            if url.scheme == "file" and url.netloc in ("", "localhost"):
                matches_app = Path(unquote(url.path)).resolve().is_relative_to(app)
            else:
                matches_app = name in (app.stem, "Corvid")
            if matches_app:
                matches.append(entry)
        # The Dock updates independently of the application's startup receipt.
        # Require the same single item on consecutive observations.
        if len(matches) == 1 and matches == previous:
            return
        previous = matches
        if time.monotonic() >= deadline:
            raise AssertionError(f"Expected one Dock item for {app}, found {matches!r}; Dock entries: {entries!r}")
        time.sleep(0.25)


def check_relaunch_entries(app, directory, dock, alias, tile):
    for entry in ("Contents/MacOS/corvid-desktop", "Contents/Helpers/corvid-relaunch"):
        check_running_relaunch(app, directory, dock, alias, entry)
        run(dock, "resolve", app, alias, tile)
        check_single_dock_tile(app)
        print(f"PASS {entry} restored the pinned Desktop with one Dock item")


def check_pinned_relaunch(binary):
    # Only change the Dock on a disposable, logged-in macOS runner.
    assert os.environ.get("GITHUB_ACTIONS") == "true"
    with tempfile.TemporaryDirectory(prefix=".cv-relaunch-", dir=Path.home()) as temp:
        directory = Path(temp).resolve()
        app = directory / "Corvid Relaunch Fixture.app"
        gui = app / "Contents/MacOS/corvid-desktop"
        gui.parent.mkdir(parents=True)
        shutil.copy2(binary, gui)
        plist(app, app.stem, gui.name, "org.corvid.test.relaunch", "1.0")
        build_relaunch_helper(app)
        sign(app)
        alias = directory / "bookmark"
        dock = directory / "dock-test"
        run("swiftc", Path(__file__).with_name("macos_dock_fixture.swift"), "-o", dock)
        tile = run(dock, "pin", app, alias)
        try:
            launch_dock_tile(dock, app, alias, tile)
            check_relaunch_entries(app, directory, dock, alias, tile)
        finally:
            run(dock, "remove", app, alias, tile)


def check_protected_bundle(app, archive, directory):
    stage = directory / "protected-update"
    stage.mkdir()
    with tarfile.open(archive) as package:
        package.extractall(stage, filter="data")
    incoming = stage / "Corvid.app"
    expected = digest(app / "Contents/Resources/package.json")
    result = subprocess.run(["codesign", "--display", "--verbose=4", incoming],
                            capture_output=True, text=True, check=True, timeout=30)
    cdhash, = [line.removeprefix("CDHash=") for line in result.stderr.splitlines()
               if line.startswith("CDHash=")]
    before = app.stat().st_ino
    run("sudo", "-n", "/usr/sbin/chown", "root:wheel", app)
    run("sudo", "-n", "/bin/chmod", "755", app)
    assert not os.access(app, os.W_OK), "fixture must require administrator write access"
    script = ROOT / "crates/corvid-update/src/macos_authorized_install.sh"
    # Hosted runners have passwordless sudo. Exercise the exact authorized
    # filesystem transaction without driving or filling the system auth panel.
    run("sudo", "-n", "/bin/sh", script, app, incoming, expected, cdhash)
    assert app.stat().st_ino == before
    assert digest(app / "Contents/Resources/package.json") == digest(incoming / "Contents/Resources/package.json")
    run("codesign", "--verify", "--deep", "--strict", app)
    print("PASS authorized replacement of an administrator-owned Corvid.app")


def check(dmg, archive):
    # Pin an isolated copy in the disposable macOS runner's real Dock.
    assert os.environ.get("GITHUB_ACTIONS") == "true"
    with (
        tempfile.TemporaryDirectory(prefix=".cv-dock-", dir=Path.home()) as temp,
        tempfile.TemporaryDirectory(prefix="Corvid Dock Fixture ", dir="/Applications") as installed,
    ):
        directory = Path(temp).resolve()
        mount = directory / "volume"
        release = release_fixture(archive, directory)
        # Match the installed basename and the bundle's display name while
        # keeping the test independent of any real installation.
        app = Path(installed) / "Corvid.app"
        protected = app.with_name("Protected Corvid.app")
        dock = directory / "dock-test"
        alias = directory / "Corvid bookmark"
        run("swiftc", Path(__file__).with_name("macos_dock_fixture.swift"), "-o", dock)
        run("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", mount, dmg)
        try:
            read_only = run_update(mount / "Corvid.app", release)
            assert read_only.returncode != 0
            assert "Copy Corvid.app from the disk image" in read_only.stderr, read_only.stderr
            run("ditto", mount / "Corvid.app", app)
            run("ditto", mount / "Corvid.app", protected)
        finally:
            run("hdiutil", "detach", mount)
        tile = None
        try:
            old = run(app / CLI, "--version")
            original_inode = app.stat().st_ino
            tile = run(dock, "pin", app, alias)
            launch_dock_tile(dock, app, alias, tile)
            run(dock, "quit", app, alias, tile)
            new = update(app, release)
            assert app.stat().st_ino == original_inode, "Dock's original app directory was replaced"
            run("codesign", "--verify", "--deep", "--strict", app)
            launch_dock_tile(dock, app, alias, tile)
            check_relaunch_entries(app, directory, dock, alias, tile)
            assert f"corvid {new}" != old, "the test needs a newer version than the published DMG"
            print(f"PASS {old} -> corvid {new}; opened the original Dock tile {tile}")
            check_protected_bundle(protected, archive, directory)
        finally:
            if tile:
                run(dock, "remove", app, alias, tile)
            if app.is_dir():
                shutil.rmtree(app)
            if protected.is_dir():
                run("sudo", "-n", "/bin/rm", "-rf", protected)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-dmg", type=Path)
    parser.add_argument("--previous-archive", type=Path)
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--relaunch-binary", type=Path)
    args = parser.parse_args()
    if args.relaunch_binary:
        check_pinned_relaunch(args.relaunch_binary)
        raise SystemExit(0)
    if not args.archive or not (args.previous_dmg or args.previous_archive):
        parser.error("provide --previous-dmg or --previous-archive")
    archive = args.archive.resolve()
    live_dock = os.environ.get("GITHUB_ACTIONS") == "true"
    with hide_recent_dock_apps() if live_dock else nullcontext():
        if args.previous_dmg:
            check(args.previous_dmg.resolve(), archive)
        if args.previous_archive:
            test_home = Path.home() if live_dock else None
            with tempfile.TemporaryDirectory(prefix=".cv-managed-", dir=test_home) as directory:
                directory = Path(directory).resolve()
                dock = directory / "dock-test"
                run("swiftc", Path(__file__).with_name("macos_dock_fixture.swift"), "-o", dock)
                check_managed_update(args.previous_archive.resolve(), archive, directory, dock, live_dock)
