#!/usr/bin/env python3
"""Exercise native Desktop install, upgrade, and removal in isolated paths."""
import argparse
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import time
import zipfile

from native_package import asset_name, build, unpack
from package import WINDOWS, digest, write_json

PREVIOUS = "0.0.0-native-test"


def command(args, **kwargs):
    return subprocess.run(args, check=True, timeout=180, **kwargs)


def previous_archive(archive, fixture):
    payload = fixture / "previous"
    metadata = unpack(archive, payload)
    metadata["version"] = PREVIOUS
    write_json(payload / "package.json", metadata)
    sums = payload / "files.sha256"
    files = sorted(file for file in payload.rglob("*") if file.is_file() and file != sums)
    sums.write_text("".join(
        f"{digest(file)}  {file.relative_to(payload).as_posix()}\n" for file in files
    ), encoding="utf-8", newline="\n")
    files.append(sums)
    stem = f"corvid-desktop-{PREVIOUS}-{metadata['target']}"
    if metadata["target"] == WINDOWS:
        previous = fixture / f"{stem}.zip"
        with zipfile.ZipFile(previous, "w", compression=zipfile.ZIP_DEFLATED) as stream:
            for file in files:
                stream.write(file, file.relative_to(payload).as_posix())
    else:
        previous = fixture / f"{stem}.tar.gz"
        with tarfile.open(previous, "w:gz") as stream:
            for file in files:
                stream.add(file, file.relative_to(payload).as_posix(), recursive=False)
    previous.with_name(previous.name + ".sha256").write_text(
        f"{digest(previous)}  {previous.name}\n", encoding="utf-8", newline="\n"
    )
    return previous, metadata["target"]


def check_payload(directory, owner):
    assert (directory / "native-package").read_text().strip() == owner
    for line in (directory / "files.sha256").read_text().splitlines():
        checksum, name = line.split("  ", 1)
        assert digest(directory / name) == checksum, name


def run_lifecycle(old_executable, new_executable, fixture, install_next):
    runtime = fixture / "run"
    state = fixture / "state"
    daemon = subprocess.Popen(
        [old_executable, "--runtime-dir", runtime, "daemon", "--state-dir", state],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        start = time.monotonic()
        while time.monotonic() - start < 10:
            probe = subprocess.run(
                [old_executable, "--runtime-dir", runtime, "control", "host"],
                capture_output=True, timeout=5,
            )
            if probe.returncode == 0:
                break
            assert daemon.poll() is None, "fixture daemon exited"
            time.sleep(0.1)
        else:
            raise AssertionError("fixture daemon did not start")
        sentinel = state / "native-install-sentinel"
        sentinel.write_text("retained\n", encoding="utf-8")
        install_next()
        after = command([new_executable, "--runtime-dir", runtime, "control", "host"],
                        capture_output=True)
        assert after.stdout == probe.stdout
        assert daemon.poll() is None
    finally:
        if daemon.poll() is None:
            daemon.terminate()
        daemon.wait(timeout=10)
    return sentinel


def windows(previous, current, fixture):
    import winreg

    key = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{6F92E7F9-5DFB-41BF-8CCD-2B746106A7D9}_is1"
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, key):
            raise ValueError("Use a clean Windows account for the native installer test")
    except FileNotFoundError:
        pass
    root = fixture / "Corvid Native Fixture"
    flags = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]

    def install(setup):
        command([setup, *flags, "/NOICONS", f"/DIR={root}"])

    install(previous)
    try:
        old = root / "versions" / PREVIOUS
        check_payload(old, "windows-setup")
        assert (root / "current.txt").read_text().strip() == PREVIOUS
        executable = root / "bin/corvid.exe"
        sentinel = run_lifecycle(old / "corvid.exe", executable, fixture, lambda: install(current))
        version = (root / "current.txt").read_text().strip()
        assert version != PREVIOUS
        check_payload(root / "versions" / version, "windows-setup")
        assert old.is_dir()
        result = command([executable, "--version"], capture_output=True)
        assert result.stdout.strip() == f"corvid {version}".encode()
    finally:
        command([root / "unins000.exe", *flags])
    assert not (root / "current.txt").exists()
    assert sentinel.read_text() == "retained\n"


def linux(previous, current, fixture):
    root = fixture / "dpkg-root"
    database = root / "var/lib/dpkg"
    database.mkdir(parents=True)
    (database / "status").touch()
    sudo = [] if os.geteuid() == 0 else ["sudo", "-n"]
    dpkg = [*sudo, "dpkg", f"--root={root}", "--force-depends"]
    try:
        command([*dpkg, "--install", previous])
        installed = root / "opt/corvid"
        check_payload(installed, "deb")
        executable = installed / "corvid"
        sentinel = run_lifecycle(executable, executable, fixture,
                                 lambda: command([*dpkg, "--install", current]))
        check_payload(installed, "deb")
        assert (root / "usr/bin/corvid-desktop").readlink() == Path("/opt/corvid/corvid-desktop")
        assert (root / "usr/share/applications/io.corvid.Desktop.desktop").is_file()
    finally:
        try:
            command([*dpkg, "--purge", "corvid-desktop"])
        finally:
            if sudo:
                command([*sudo, "chown", "-R", f"{os.getuid()}:{os.getgid()}", root])
    assert not installed.exists()
    assert sentinel.read_text() == "retained\n"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--iscc", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="corvid-native-test-", dir=Path.home() if os.name != "nt" else None) as directory:
        fixture = Path(directory)
        archive, target = previous_archive(args.archive, fixture)
        build(archive, fixture, args.iscc)
        previous = fixture / asset_name(PREVIOUS, target)
        (windows if target == WINDOWS else linux)(previous, args.package.resolve(), fixture)
    print("PASS native install, upgrade with live daemon, and uninstall")
