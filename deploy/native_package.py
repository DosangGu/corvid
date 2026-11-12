#!/usr/bin/env python3
"""Build Windows Setup from a checked Corvid Desktop archive."""
import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tempfile
import zipfile

from package import ROOT, WINDOWS, digest, write_json

ICON = ROOT / "assets/icons/corvid.ico"
MAX_PAYLOAD_BYTES = 512 * 1024 * 1024
MAX_MEMBERS = 256


def asset_name(version):
    return f"corvid-desktop-{version}-{WINDOWS}-setup.exe"


def unpack(archive, payload):
    checksum = archive.with_name(archive.name + ".sha256").read_text().split()
    if checksum != [digest(archive), archive.name]:
        raise ValueError("Desktop archive checksum mismatch")
    seen = set()
    total = 0

    def write(name, size, stream):
        nonlocal total
        path = PurePosixPath(name)
        if name != path.as_posix() or path.is_absolute() or any(
            not re.fullmatch(r"[a-zA-Z0-9_-][a-zA-Z0-9_.-]*", part)
            or part.endswith(".")
            or re.fullmatch(r"CON|PRN|AUX|NUL|(?:COM|LPT)[1-9]", part.split(".")[0], re.I)
            for part in path.parts
        ):
            raise ValueError(f"Invalid archive path: {name}")
        if name.lower() in seen or len(seen) == MAX_MEMBERS:
            raise ValueError("Invalid archive member count or duplicate path")
        seen.add(name.lower())
        total += size
        if total > MAX_PAYLOAD_BYTES:
            raise ValueError("Desktop archive exceeds payload limit")
        destination = payload.joinpath(*path.parts)
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("xb") as output:
            shutil.copyfileobj(stream, output)

    with zipfile.ZipFile(archive) as package:
        for member in package.infolist():
            if member.is_dir() or stat.S_ISLNK(member.external_attr >> 16):
                raise ValueError("Expected regular files in Desktop archive")
            with package.open(member) as stream:
                write(member.filename, member.file_size, stream)

    sums = {}
    for line in (payload / "files.sha256").read_text(encoding="utf-8").splitlines():
        checksum, name = line.split("  ", 1)
        if name in sums:
            raise ValueError("Duplicate payload checksum")
        sums[name] = checksum
    files = {
        file.relative_to(payload).as_posix(): digest(file)
        for file in payload.rglob("*")
        if file.is_file() and file != payload / "files.sha256"
    }
    if sums != files:
        raise ValueError("Desktop payload integrity mismatch")
    return json.loads((payload / "package.json").read_text(encoding="utf-8"))


def stamp(payload):
    (payload / "native-package").write_text("windows-setup\n", encoding="utf-8", newline="\n")
    with (payload / "files.sha256").open("a", encoding="utf-8", newline="\n") as sums:
        sums.write(f"{digest(payload / 'native-package')}  native-package\n")


def windows_package(payload, version, output, compiler):
    if os.name != "nt":
        raise ValueError("Build Windows Setup on Windows")
    if compiler is None:
        compiler = shutil.which("ISCC.exe")
    if compiler is None:
        raise ValueError("Specify --iscc with the path to the Inno Setup compiler")
    for file in ("corvid-launch.exe", "corvid-desktop-launch.exe", "corvid-agent-deck-launch.exe",
                 "plugins/agent-deck/corvid-agent-deck.exe"):
        if not (payload / file).is_file():
            raise ValueError(f"Desktop archive is missing {file}")
    stamp(payload)

    subprocess.run([
        str(compiler), f"/DRelease={version}", f"/DPayload={payload}",
        f"/DIcon={ICON}", f"/O{output}", ROOT / "deploy/windows/CorvidSetup.iss",
    ], check=True)


def build(archive, output, compiler):
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="corvid-native-") as temporary:
        payload = Path(temporary) / "payload"
        metadata = unpack(archive, payload)
        version, revision = (metadata[key] for key in ("version", "source_revision"))
        if (metadata.get("schema_version") != 1 or metadata.get("product") != "desktop"
                or metadata.get("target") != WINDOWS
                or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", version)
                or not re.fullmatch(r"[a-f0-9]{40}", revision)
                or archive.name != f"corvid-desktop-{version}-{WINDOWS}.zip"):
            raise ValueError("Desktop archive identity mismatch")
        artifact = output / asset_name(version)
        windows_package(payload, version, output, compiler)
    checksum = digest(artifact)
    artifact.with_name(artifact.name + ".sha256").write_text(
        f"{checksum}  {artifact.name}\n", encoding="utf-8", newline="\n"
    )
    write_json(artifact.with_name(artifact.name + ".provenance.json"), {
        "schema_version": 1, "source_revision": revision, "version": version,
        "target": WINDOWS, "archive": archive.name, "archive_sha256": digest(archive),
        "sha256": checksum, "size": artifact.stat().st_size,
    })
    print(artifact)


def verify(directory, version, revision):
    name = asset_name(version)
    artifact = directory / name
    archive = f"corvid-desktop-{version}-{WINDOWS}.zip"
    checksum = digest(artifact)
    expected = {
        "schema_version": 1, "source_revision": revision, "version": version,
        "target": WINDOWS, "archive": archive, "archive_sha256": digest(directory / archive),
        "sha256": checksum, "size": artifact.stat().st_size,
    }
    provenance = json.loads(artifact.with_name(name + ".provenance.json").read_text())
    sidecar = artifact.with_name(name + ".sha256").read_text().split()
    if provenance != expected or sidecar != [checksum, name]:
        raise ValueError(f"Native package provenance mismatch: {name}")
    return name


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--iscc", type=Path)
    args = parser.parse_args()
    build(args.archive.resolve(), args.output.resolve(), args.iscc)
