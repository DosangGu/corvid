#!/usr/bin/env python3
"""Build a native Desktop package from a checked Corvid release archive."""
import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import zipfile

from package import DESKTOP_LINUX, ROOT, WINDOWS, copy, digest, write_json

ICON = ROOT / "plugins/agent-deck/assets/app.ico"
MAX_PAYLOAD_BYTES = 512 * 1024 * 1024
MAX_MEMBERS = 256


def asset_name(version, target):
    if target == WINDOWS:
        return f"corvid-desktop-{version}-{WINDOWS}-setup.exe"
    if target == DESKTOP_LINUX:
        return f"corvid-desktop_{version}_amd64.deb"
    raise ValueError("Unsupported native package platform")


def debian_version(version):
    # Debian sorts a release after its prereleases when they start with '~'.
    return version.replace("-", "~", 1)


def unpack(archive, payload):
    checksum = archive.with_name(archive.name + ".sha256").read_text().split()
    if checksum != [digest(archive), archive.name]:
        raise ValueError("Desktop archive checksum mismatch")
    seen = set()
    total = 0

    def write(name, size, mode, stream):
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
        destination.chmod(0o755 if mode & 0o111 else 0o644)

    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as package:
            for member in package.infolist():
                mode = member.external_attr >> 16
                if member.is_dir() or stat.S_ISLNK(mode):
                    raise ValueError("Expected regular files in Desktop archive")
                with package.open(member) as stream:
                    write(member.filename, member.file_size, mode, stream)
    else:
        with tarfile.open(archive, "r:gz") as package:
            for member in package:
                if not member.isfile():
                    raise ValueError("Expected regular files in Desktop archive")
                with package.extractfile(member) as stream:
                    write(member.name, member.size, member.mode, stream)

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


def stamp(payload, owner):
    (payload / "native-package").write_text(owner + "\n", encoding="utf-8", newline="\n")
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
    stamp(payload, "windows-setup")

    subprocess.run([
        str(compiler), f"/DRelease={version}", f"/DPayload={payload}",
        f"/DIcon={ICON}", f"/O{output}", ROOT / "deploy/windows/CorvidSetup.iss",
    ], check=True)


def minimum_glibc(paths):
    versions = [(2, 39)]
    for binary in paths:
        headers = subprocess.check_output(["readelf", "--version-info", str(binary)], text=True)
        versions.extend(tuple(map(int, value.split("."))) for value in re.findall(
            r"Name: GLIBC_(\d+(?:\.\d+)+)", headers
        ))
    return ".".join(map(str, max(versions)))


def debian_package(payload, stage, version, artifact):
    deck = payload / "plugins/agent-deck/corvid-agent-deck"
    glibc = minimum_glibc([payload / "corvid-desktop", deck])
    stamp(payload, "deb")
    install = stage / "opt/corvid"
    install.parent.mkdir(parents=True)
    shutil.move(payload, install)
    bin_dir = stage / "usr/bin"
    bin_dir.mkdir(parents=True)
    for name in ("corvid", "corvid-desktop"):
        (bin_dir / name).symlink_to(f"/opt/corvid/{name}")
    share = stage / "usr/share"
    copy(ROOT / "deploy/linux/io.corvid.Desktop.desktop", share / "applications/io.corvid.Desktop.desktop")
    copy(ROOT / "plugins/agent-deck/assets/app.png", share / "icons/hicolor/256x256/apps/io.corvid.Desktop.png")
    copy(ROOT / "LICENSE", share / "doc/corvid-desktop/copyright")
    control = stage / "DEBIAN"
    control.mkdir()
    (control / "control").write_text(
        f"Package: corvid-desktop\nVersion: {debian_version(version)}\n"
        "Architecture: amd64\nSection: utils\nPriority: optional\n"
        "Maintainer: Corvid project\n"
        f"Depends: libc6 (>= {glibc}), libgcc-s1, libfontconfig1, libx11-6, "
        "libxcb1, libxkbcommon0, libxkbcommon-x11-0, libvulkan1, "
        "libxcursor1, libxi6, libxrandr2, "
        "libegl1, libgl1, libwayland-client0\n"
        "Recommends: mesa-vulkan-drivers, libnotify-bin, openssh-client\n"
        "Homepage: https://github.com/DosangGu/corvid\n"
        "Description: Terminal workspace for coding agents\n"
        " Includes the Corvid CLI, Desktop, Agent Deck, and remote daemon payloads.\n",
        encoding="utf-8",
    )
    subprocess.run(["dpkg-deb", "--root-owner-group", "--build", stage, artifact], check=True)


def build(archive, output, compiler):
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="corvid-native-") as temporary:
        root = Path(temporary)
        payload = root / "payload"
        metadata = unpack(archive, payload)
        version, revision, target = (metadata[key] for key in ("version", "source_revision", "target"))
        extension = "zip" if target == WINDOWS else "tar.gz"
        if (metadata.get("schema_version") != 1 or metadata.get("product") != "desktop"
                or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", version)
                or not re.fullmatch(r"[a-f0-9]{40}", revision)
                or archive.name != f"corvid-desktop-{version}-{target}.{extension}"):
            raise ValueError("Desktop archive identity mismatch")
        artifact = output / asset_name(version, target)
        if target == WINDOWS:
            windows_package(payload, version, output, compiler)
        else:
            debian_package(payload, root / "deb", version, artifact)
    checksum = digest(artifact)
    artifact.with_name(artifact.name + ".sha256").write_text(
        f"{checksum}  {artifact.name}\n", encoding="utf-8", newline="\n"
    )
    write_json(artifact.with_name(artifact.name + ".provenance.json"), {
        "schema_version": 1, "source_revision": revision, "version": version,
        "target": target, "archive": archive.name, "archive_sha256": digest(archive),
        "sha256": checksum, "size": artifact.stat().st_size,
    })
    print(artifact)


def verify(directory, version, revision, target):
    name = asset_name(version, target)
    artifact = directory / name
    extension = "zip" if target == WINDOWS else "tar.gz"
    archive = f"corvid-desktop-{version}-{target}.{extension}"
    checksum = digest(artifact)
    expected = {
        "schema_version": 1, "source_revision": revision, "version": version,
        "target": target, "archive": archive, "archive_sha256": digest(directory / archive),
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
