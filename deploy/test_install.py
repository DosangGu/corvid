#!/usr/bin/env python3
"""Exercise real installers with local release assets in disposable user directories."""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import time
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
WINDOWS = os.name == "nt"
FLAGS = {"creationflags": subprocess.CREATE_NO_WINDOW} if WINDOWS else {}


def run(arguments, success=True, **kwargs):
    result = subprocess.run([str(value) for value in arguments], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=90, **FLAGS, **kwargs)
    if (result.returncode == 0) != success:
        raise AssertionError(f"Unexpected exit {result.returncode}: {arguments}\n{result.stdout}\n{result.stderr}")
    return result


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def previous_fixture(assets, archive, temporary):
    """A version-probe fixture tests switching versions, not old daemon compatibility."""
    stage = temporary / "fixture"
    stage.mkdir()
    if WINDOWS:
        with zipfile.ZipFile(archive) as stream:
            stream.extractall(stage)
    else:
        with tarfile.open(archive) as stream:
            stream.extractall(stage, filter="data")
    source = temporary / "previous.rs"
    source.write_text('fn main() { println!("corvid 0.0.0"); }\n')
    run(["rustc", "--crate-name", "previous", source, "-o", stage / ("corvid.exe" if WINDOWS else "corvid")])
    metadata = json.loads((stage / "package.json").read_text())
    metadata["version"] = "0.0.0"
    (stage / "package.json").write_text(json.dumps(metadata, indent=2) + "\n")
    files = sorted(path for path in stage.rglob("*") if path.is_file() and path.name != "files.sha256")
    (stage / "files.sha256").write_text("".join(f"{sha256(path)}  {path.relative_to(stage).as_posix()}\n" for path in files), newline="\n")
    files.append(stage / "files.sha256")
    old = assets / f"corvid-{metadata['product']}-0.0.0-{metadata['target']}.{'zip' if WINDOWS else 'tar.gz'}"
    if WINDOWS:
        with zipfile.ZipFile(old, "w", zipfile.ZIP_DEFLATED) as stream:
            for path in files:
                stream.write(path, path.relative_to(stage).as_posix())
    else:
        with tarfile.open(old, "w:gz") as stream:
            for path in files:
                stream.add(path, path.relative_to(stage).as_posix(), recursive=False)
    old.with_name(old.name + ".sha256").write_text(f"{sha256(old)}  {old.name}\n")


@contextlib.contextmanager
def installer_lock(root):
    path = root / ".install-lock"
    if WINDOWS:
        script = "$f=[IO.File]::Open($args[0], 'OpenOrCreate', 'ReadWrite', 'None'); [Console]::WriteLine('locked'); [Console]::ReadLine() | Out-Null; $f.Dispose()"
        # The path is passed through an environment variable to avoid PowerShell command parsing.
        script = script.replace("$args[0]", "$env:CORVID_TEST_LOCK")
        environment = dict(os.environ, CORVID_TEST_LOCK=str(path))
        process = subprocess.Popen(["powershell.exe", "-NoProfile", "-Command", script], env=environment,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, **FLAGS)
        try:
            assert process.stdout.readline().strip() == "locked"
            yield
        finally:
            process.communicate("\n", timeout=10)
    else:
        path.mkdir()
        try:
            yield
        finally:
            path.rmdir()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--product", choices=["tui", "desktop"], default="tui")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if not WINDOWS:
        args.binary.chmod(0o755)
    assert run([args.binary.resolve(), "--version"]).stdout.strip() == f"corvid {version}"
    target = "x86_64-pc-windows-msvc" if WINDOWS else "x86_64-unknown-linux-" + ("musl" if args.product == "tui" else "gnu")
    name = f"corvid-{args.product}-{version}-{target}.{'zip' if WINDOWS else 'tar.gz'}"
    # Unix runtime validation deliberately rejects world-writable ancestors such as /tmp.
    with tempfile.TemporaryDirectory(prefix=".corvid-install-test-", dir=None if WINDOWS else Path.home()) as temporary:
        temporary = Path(temporary)
        assets = temporary / "assets"
        assets.mkdir()
        archive = assets / name
        shutil.copyfile(args.assets / name, archive)
        shutil.copyfile(args.assets / (name + ".sha256"), assets / (name + ".sha256"))
        previous_fixture(assets, archive, temporary)
        root = temporary / "installed with spaces"
        bin_dir = root / "bin" if WINDOWS else temporary / "bin with spaces"
        data_home = temporary / "desktop data"

        def install(selected=version, success=True):
            if WINDOWS:
                arguments = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", ROOT / "deploy/install.ps1", "-Version", selected,
                             "-Product", args.product, "-InstallRoot", root, "-SourceDirectory", assets]
            else:
                arguments = ["sh", ROOT / "deploy/install.sh", "--version", selected, "--product", args.product,
                             "--install-root", root, "--bin-dir", bin_dir, "--source-dir", assets]
            return run(arguments, success, env=dict(os.environ, XDG_DATA_HOME=str(data_home)))

        def installed_version():
            return run([bin_dir / ("corvid.exe" if WINDOWS else "corvid"), "--version"]).stdout.strip()

        install("0.0.0")
        assert installed_version() == "corvid 0.0.0"
        install()
        assert installed_version() == f"corvid {version}"
        assert (root / "versions/0.0.0").is_dir(), "Previous executables must be retained"
        if WINDOWS:
            assert (root / "corvid.ico").read_bytes() == (ROOT / "assets/icons/corvid.ico").read_bytes()
        elif args.product == "desktop":
            entry = data_home / "applications/Corvid.Desktop.desktop"
            contents = entry.read_text()
            assert f'Exec="{root}/current/corvid-desktop"' in contents
            assert "StartupWMClass=Corvid.Desktop" in contents
            assert f"Icon={root}/current/icons/corvid.svg" in contents
            assert (data_home / "icons/hicolor/scalable/apps/corvid.svg").resolve() == root / "versions" / version / "icons/corvid.svg"
            if shutil.which("desktop-file-validate"):
                run(["desktop-file-validate", entry])
        install()
        with installer_lock(root):
            install(success=False)
        assert installed_version() == f"corvid {version}"

        installed = root / "versions" / version / ("corvid.exe" if WINDOWS else "corvid")
        runtime = temporary / "run"
        state = temporary / "state"
        daemon = subprocess.Popen([str(installed), "--runtime-dir", str(runtime), "daemon", "--state-dir", str(state)],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, **FLAGS)
        try:
            for _ in range(80):
                ping = subprocess.run([str(installed), "--runtime-dir", str(runtime), "control", "ping"],
                                      capture_output=True, timeout=10, **FLAGS)
                if ping.returncode == 0:
                    break
                if daemon.poll() is not None:
                    raise AssertionError(daemon.stderr.read().decode(errors="replace"))
                time.sleep(0.1)
            else:
                raise AssertionError("Packaged daemon did not answer ping")
            install()
            with archive.open("ab") as stream:
                stream.write(b"corrupt")
            install(success=False)
            assert installed_version() == f"corvid {version}"
            assert daemon.poll() is None, "Installer must not restart a running daemon"
            run([installed, "--runtime-dir", runtime, "control", "ping"])
        finally:
            daemon.terminate()
            daemon.communicate(timeout=15)
        print(f"PASS {args.product}: fresh install, version switch, reinstall, concurrent lock, corrupt download, running daemon")


if __name__ == "__main__":
    main()
