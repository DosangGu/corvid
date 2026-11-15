#!/usr/bin/env python3
"""Build an ad-hoc signed Apple Silicon app and an unsigned drag-to-install DMG."""
import argparse
import json
import plistlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

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


def app_bundle(stage, build, version):
    if sys.platform != "darwin":
        raise ValueError("Build the macOS app on macOS")
    app = stage / "Corvid.app"
    binaries = app / "Contents/MacOS"
    binaries.mkdir(parents=True)
    for name in ["corvid", "corvid-desktop"]:
        shutil.move(stage / name, binaries / name)
    plist(app, "Corvid", "corvid-desktop", "org.corvid.desktop", version)
    copy(stage / "package.json", app / "Contents/Resources/package.json")
    shutil.move(stage / "daemon", app / "Contents/Resources/daemon")
    icon(app / "Contents/Resources/corvid.icns")
    copy(stage / "LICENSE", app / "Contents/Resources/LICENSE")
    sign(binaries / "corvid")
    # Remote runtime manifests describe the exact transferred bytes. Do not re-sign them.
    plugin = stage / "plugins/agent-deck/corvid-agent-deck"
    if plugin.exists():
        helper = app / "Contents/Helpers/Corvid Agent Deck.app"
        copy(plugin, helper / "Contents/MacOS/corvid-agent-deck", True)
        plist(helper, "Corvid Agent Deck", "corvid-agent-deck", "org.corvid.agent-deck", version, True)
        copy(app / "Contents/Resources/corvid.icns", helper / "Contents/Resources/corvid.icns")
        sign(helper)
        shutil.rmtree(stage / "plugins")
    for name in ["corvid-deck", "corvid-streamdeck"]:
        if (build / name).is_file():
            copy(build / name, binaries / name, True)
            sign(binaries / name)
    shutil.rmtree(stage / "icons")
    sign(app)
    run("codesign", "--verify", "--deep", "--strict", app)


def asset_name(version):
    return f"corvid-desktop-{version}-{MACOS}.dmg"


def build(archive, output):
    if sys.platform != "darwin":
        raise ValueError("Build DMGs on macOS")
    if archive.with_name(archive.name + ".sha256").read_text().split() != [digest(archive), archive.name]:
        raise ValueError("Desktop archive checksum mismatch")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="corvid-dmg-") as temporary:
        stage = Path(temporary)
        with tarfile.open(archive) as stream:
            for member in stream.getmembers():
                if not member.isfile():
                    raise ValueError("Expected regular archive files")
            stream.extractall(stage, filter="data")
        metadata = json.loads((stage / "package.json").read_text())
        version, revision = metadata["version"], metadata["source_revision"]
        if metadata["product"] != "desktop" or metadata["target"] != MACOS:
            raise ValueError("Expected an Apple Silicon Desktop archive")
        for line in (stage / "files.sha256").read_text().splitlines():
            checksum, name = line.split("  ", 1)
            if digest(stage / name) != checksum:
                raise ValueError("Desktop payload checksum mismatch")
        app = stage / "Corvid.app"
        run("codesign", "--verify", "--deep", "--strict", app)
        volume = stage / "volume"
        volume.mkdir()
        shutil.move(app, volume / app.name)
        (volume / "Applications").symlink_to("/Applications")
        artifact = output / asset_name(version)
        run("hdiutil", "create", "-ov", "-volname", "Corvid", "-srcfolder", volume,
            "-format", "UDZO", artifact)
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
