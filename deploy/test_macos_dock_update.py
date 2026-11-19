#!/usr/bin/env python3
"""Update a published DMG copy and reopen its pre-update Dock reference on macOS."""

import argparse
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import time
import uuid

from package import digest

ROOT = Path(__file__).resolve().parents[1]
CLI = "Contents/MacOS/corvid"
BUNDLE = "org.corvid.desktop"


def run(*args, **kwargs):
    return subprocess.check_output([str(arg) for arg in args], text=True, timeout=120, **kwargs).strip()


def update(app, archive, directory):
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
    result = subprocess.run([app / CLI, "update", "--version", version], env=env,
                            capture_output=True, text=True, timeout=120)
    assert result.returncode == 0, result.stdout + result.stderr
    assert not (fixture_home / "Applications/Corvid.app").exists(), result.stdout
    metadata = json.loads((app / "Contents/Resources/package.json").read_text())
    assert metadata["source_revision"] == revision and metadata["version"] == version
    assert run(app / CLI, "--version") == f"corvid {version}"
    return version


def check(dmg, archive):
    # Pin an isolated copy in the disposable macOS runner's real Dock.
    assert os.environ.get("GITHUB_ACTIONS") == "true"
    with tempfile.TemporaryDirectory(prefix=".cv-dock-", dir=Path.home()) as temp:
        directory = Path(temp).resolve()
        mount = directory / "volume"
        app = Path("/Applications") / f"Corvid Dock Fixture {uuid.uuid4().hex[:10]}.app"
        dock = directory / "dock-test"
        alias = directory / "Corvid bookmark"
        run("swiftc", Path(__file__).with_name("macos_dock_fixture.swift"), "-o", dock)
        run("hdiutil", "attach", "-readonly", "-nobrowse", "-mountpoint", mount, dmg)
        try:
            run("ditto", mount / "Corvid.app", app)
        finally:
            run("hdiutil", "detach", mount)
        try:
            old = run(app / CLI, "--version")
            original_inode = app.stat().st_ino
            tile = run(dock, "pin", app, alias)
            run(dock, "launch", app, alias, tile)
            run(dock, "quit", app, alias, tile)
            new = update(app, archive, directory)
            assert app.stat().st_ino == original_inode, "Dock's original app directory was replaced"
            run("codesign", "--verify", "--deep", "--strict", app)
            method = run(dock, "launch", app, alias, tile)
            assert f"corvid {new}" != old, "the test needs a newer version than the published DMG"
            print(f"PASS {old} → corvid {new}; original Dock tile {tile}; opened via {method}")
        finally:
            run(dock, "remove", app, alias)
            if app.is_dir():
                shutil.rmtree(app)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-dmg", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    args = parser.parse_args()
    check(args.previous_dmg.resolve(), args.archive.resolve())
