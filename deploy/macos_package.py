#!/usr/bin/env python3
"""Build an ad-hoc signed Apple Silicon app and an unsigned drag-to-install DMG."""
import argparse
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path, PurePosixPath

from package import MACOS, ROOT, copy, digest, write_json


def run(*args):
    subprocess.run([str(arg) for arg in args], check=True)


def plist(app, name, executable, identifier, version, background=False):
    resources = app / "Contents/Resources"
    resources.mkdir(parents=True)
    with (app / "Contents/Info.plist").open("wb") as output:
        plistlib.dump({
            "CFBundleName": name, "CFBundleDisplayName": name,
            "CFBundleIdentifier": identifier, "CFBundleExecutable": executable,
            "CFBundlePackageType": "APPL", "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleShortVersionString": version.split("-")[0],
            "CFBundleVersion": version.split("-")[0], "CFBundleIconFile": "corvid.icns",
            "LSMinimumSystemVersion": "15.0", "LSUIElement": background,
            "NSHighResolutionCapable": True,
        }, output)


def icon(destination):
    with tempfile.TemporaryDirectory() as temporary:
        iconset = Path(temporary) / "corvid.iconset"
        iconset.mkdir()
        for size in [16, 32, 128, 256, 512]:
            for scale in [1, 2]:
                name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
                run("sips", "-z", size * scale, size * scale, ROOT / "assets/icons/corvid.png",
                    "--out", iconset / name)
        run("iconutil", "-c", "icns", "-o", destination, iconset)


def sign(path):
    run("codesign", "--force", "--sign", "-", "--timestamp=none", path)


def build_relaunch_helper(app):
    relaunch = app / "Contents/Helpers/corvid-relaunch"
    relaunch.parent.mkdir(parents=True, exist_ok=True)
    run("xcrun", "swiftc", "-O", "-parse-as-library", "-target", "arm64-apple-macosx15.0",
        ROOT / "deploy/macos/relaunch.swift", "-o", relaunch)
    sign(relaunch)


def app_bundle(stage, version):
    if sys.platform != "darwin":
        raise ValueError("Build the macOS app on macOS")
    app = stage / "Corvid.app"
    binaries = app / "Contents/MacOS"
    binaries.mkdir(parents=True)
    for name in ["corvid", "corvidd", "corvid-desktop"]:
        shutil.move(stage / name, binaries / name)
    plist(app, "Corvid", "corvid-desktop", "org.corvid.desktop", version)
    copy(stage / "package.json", app / "Contents/Resources/package.json")
    shutil.move(stage / "daemon", app / "Contents/Resources/daemon")
    icon(app / "Contents/Resources/corvid.icns")
    copy(stage / "LICENSE", app / "Contents/Resources/LICENSE")
    copy(stage / "STIX-OFL.txt", app / "Contents/Resources/STIX-OFL.txt")
    sign(binaries / "corvid")
    sign(binaries / "corvidd")
    build_relaunch_helper(app)
    # Remote runtime manifests describe the exact transferred bytes. Do not re-sign them.
    plugin = stage / "plugins/agent-deck/corvid-agent-deck"
    if plugin.exists():
        helper = app / "Contents/Helpers/Corvid Agent Deck.app"
        copy(plugin, helper / "Contents/MacOS/corvid-agent-deck", True)
        plist(helper, "Corvid Agent Deck", "corvid-agent-deck", "org.corvid.agent-deck", version, True)
        copy(app / "Contents/Resources/corvid.icns", helper / "Contents/Resources/corvid.icns")
        sign(helper)
        shutil.rmtree(stage / "plugins")
    shutil.rmtree(stage / "icons")
    sign(app)
    run("codesign", "--verify", "--deep", "--strict", app)


def asset_name(version):
    return f"corvid-desktop-{version}-{MACOS}.dmg"


def create_dmg(volume, artifact):
    command = [
        "hdiutil", "create", "-ov", "-volname", "Corvid", "-srcfolder", str(volume),
        "-format", "UDZO", str(artifact),
    ]
    # DiskImages can briefly fail to obtain exclusive access on hosted macOS runners.
    for attempt in range(3):
        result = subprocess.run(
            command, stderr=subprocess.PIPE, text=True, env=dict(os.environ, LC_ALL="C"),
        )
        print(result.stderr, end="", file=sys.stderr, flush=True)
        if result.returncode == 0:
            return
        if "hdiutil: create failed - Resource busy" not in result.stderr or attempt == 2:
            result.check_returncode()
        artifact.unlink(missing_ok=True)
        delay = 5 * (attempt + 1)
        print(f"Retrying DMG creation in {delay}s (attempt {attempt + 2}/3)",
              file=sys.stderr, flush=True)
        time.sleep(delay)


def build(archive, output):
    if sys.platform != "darwin":
        raise ValueError("Build DMGs on macOS")
    if archive.with_name(archive.name + ".sha256").read_text().split() != [digest(archive), archive.name]:
        raise ValueError("Desktop archive checksum mismatch")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="corvid-dmg-") as temporary:
        stage = Path(temporary)
        with tarfile.open(archive) as stream:
            seen = set()
            total = 0
            for member in stream.getmembers():
                path = PurePosixPath(member.name)
                if (not member.isfile() or path.is_absolute() or ".." in path.parts
                        or member.name != path.as_posix() or member.name in seen
                        or any(not re.fullmatch(r"[a-zA-Z0-9_. -]+", part) for part in path.parts)):
                    raise ValueError("Invalid macOS archive member")
                seen.add(member.name)
                total += member.size
                if len(seen) > 1024 or total > 1024 * 1024 * 1024:
                    raise ValueError("macOS archive exceeds payload limits")
            stream.extractall(stage, filter="data")
        metadata = json.loads((stage / "package.json").read_text())
        version, revision = metadata["version"], metadata["source_revision"]
        if (metadata.get("schema_version") != 1 or metadata["product"] != "desktop"
                or metadata["target"] != MACOS or not re.fullmatch(r"[a-f0-9]{40}", revision)
                or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", version)
                or archive.name != f"corvid-desktop-{version}-{MACOS}.tar.gz"):
            raise ValueError("Expected an identified Apple Silicon Desktop archive")
        checksums = {}
        for line in (stage / "files.sha256").read_text().splitlines():
            checksum, name = line.split("  ", 1)
            if name in checksums:
                raise ValueError("Duplicate macOS payload checksum")
            checksums[name] = checksum
        actual = {path.relative_to(stage).as_posix(): digest(path)
                  for path in stage.rglob("*") if path.is_file() and path != stage / "files.sha256"}
        if checksums != actual:
            raise ValueError("Desktop payload checksum mismatch")
        app = stage / "Corvid.app"
        run("codesign", "--verify", "--deep", "--strict", app)
        volume = stage / "volume"
        volume.mkdir()
        shutil.move(app, volume / app.name)
        (volume / "Applications").symlink_to("/Applications")
        artifact = output / asset_name(version)
        create_dmg(volume, artifact)
    checksum = digest(artifact)
    artifact.with_name(artifact.name + ".sha256").write_text(f"{checksum}  {artifact.name}\n")
    write_json(artifact.with_name(artifact.name + ".provenance.json"), {
        "schema_version": 1, "source_revision": revision, "version": version,
        "target": MACOS, "archive": archive.name, "archive_sha256": digest(archive),
        "sha256": checksum, "size": artifact.stat().st_size,
    })


def verify(directory, version, revision):
    name = asset_name(version)
    artifact = directory / name
    archive = f"corvid-desktop-{version}-{MACOS}.tar.gz"
    checksum = digest(artifact)
    expected = {
        "schema_version": 1, "source_revision": revision, "version": version,
        "target": MACOS, "archive": archive, "archive_sha256": digest(directory / archive),
        "sha256": checksum, "size": artifact.stat().st_size,
    }
    if (json.loads(artifact.with_name(name + ".provenance.json").read_text()) != expected
            or artifact.with_name(name + ".sha256").read_text().split() != [checksum, name]):
        raise ValueError("DMG provenance mismatch")
    return name


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    build(args.archive.resolve(), args.output.resolve())
