#!/usr/bin/env python3
"""Assemble release assets from previously built, revision-matched runtimes."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
LINUX = "x86_64-unknown-linux-musl"
WINDOWS = "x86_64-pc-windows-msvc"
DESKTOP_LINUX = "x86_64-unknown-linux-gnu"


def identity():
    inputs = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo", "crates", "plugins", "spikes", "deploy"]
    changes = subprocess.check_output(
        ["git", "-c", "core.safecrlf=false", "status", "--porcelain", "--untracked-files=normal", "--", *inputs],
        cwd=ROOT, text=True,
    )
    if changes:
        raise ValueError("Commit the build inputs before packaging a release")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    return version, revision


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8", newline="\n")


def copy(source, destination, executable=False):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(0o755 if executable else 0o644)


def runtime(args):
    version, revision = identity()
    binary = args.binary.resolve()
    output = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if output != f"corvid {version}":
        raise ValueError("Runtime version differs from the workspace")
    if not 0 < binary.stat().st_size <= 32 * 1024 * 1024:
        raise ValueError("Runtime exceeds the daemon transfer limit")
    if args.target == LINUX:
        headers = subprocess.check_output(["readelf", "--wide", "--program-headers", str(binary)], text=True)
        dynamic = subprocess.check_output(["readelf", "--wide", "--dynamic", str(binary)], text=True)
        if " INTERP " in headers or "(NEEDED)" in dynamic:
            raise ValueError("Linux runtime must be statically linked")
    args.output.mkdir(parents=True, exist_ok=True)
    name = "corvid.exe" if args.target == WINDOWS else "corvid"
    copy(binary, args.output / name, True)
    write_json(args.output / "manifest.json", {
        "schema_version": 1, "target": args.target, "version": version,
        "source_revision": revision, "size": binary.stat().st_size, "sha256": digest(binary),
    })


def verify_runtime(directory, target, version, revision):
    manifest = json.loads((directory / "manifest.json").read_text(encoding="utf-8"))
    binary = directory / ("corvid.exe" if target == WINDOWS else "corvid")
    expected = {"schema_version": 1, "target": target, "version": version, "source_revision": revision}
    if any(manifest.get(key) != value for key, value in expected.items()):
        raise ValueError(f"Runtime identity mismatch: {directory}")
    if manifest["size"] != binary.stat().st_size or manifest["sha256"] != digest(binary):
        raise ValueError(f"Runtime integrity mismatch: {directory}")
    return binary


def package(args):
    version, revision = identity()
    windows = args.target == WINDOWS
    native = verify_runtime(args.runtime, WINDOWS if windows else LINUX, version, revision)
    suffix = ".exe" if windows else ""
    extension = "zip" if windows else "tar.gz"
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="corvid-package-") as temporary:
        stage = Path(temporary)
        copy(native, stage / f"corvid{suffix}", True)
        copy(ROOT / "LICENSE", stage / "LICENSE")
        if windows:
            copy(args.build / "corvid-launch.exe", stage / "corvid-launch.exe", True)
        if args.product == "desktop":
            copy(args.build / f"corvid-desktop{suffix}", stage / f"corvid-desktop{suffix}", True)
            if not args.without_agent_deck:
                copy(args.build / f"corvid-agent-deck{suffix}", stage / "plugins" / "agent-deck" / f"corvid-agent-deck{suffix}", True)
            if windows:
                copy(args.build / "corvid-desktop-launch.exe", stage / "corvid-desktop-launch.exe", True)
                if not args.without_agent_deck:
                    copy(args.build / "corvid-agent-deck-launch.exe", stage / "corvid-agent-deck-launch.exe", True)
                copy(args.build / "corvid-deck.exe", stage / "corvid-deck.exe", True)
                copy(ROOT / "deploy/windows/Set-CorvidStartup.ps1", stage / "Set-CorvidStartup.ps1")
            for target, directory in [(LINUX, args.linux_runtime), (WINDOWS, args.windows_runtime)]:
                if directory is None:
                    raise ValueError(f"Desktop requires the {target} runtime")
                binary = verify_runtime(directory, target, version, revision)
                copy(binary, stage / "daemon" / target / binary.name, True)
                copy(directory / "manifest.json", stage / "daemon" / target / "manifest.json")
        write_json(stage / "package.json", {
            "schema_version": 1, "version": version, "source_revision": revision,
            "product": args.product, "target": args.target,
        })
        files = sorted(path for path in stage.rglob("*") if path.is_file())
        (stage / "files.sha256").write_text("".join(f"{digest(path)}  {path.relative_to(stage).as_posix()}\n" for path in files), encoding="utf-8", newline="\n")
        archive = args.output / f"corvid-{args.product}-{version}-{args.target}.{extension}"
        files.append(stage / "files.sha256")
        if windows:
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as stream:
                for path in files:
                    stream.write(path, path.relative_to(stage).as_posix())
        else:
            with tarfile.open(archive, "w:gz") as stream:
                for path in files:
                    stream.add(path, path.relative_to(stage).as_posix(), recursive=False)
        archive.with_name(archive.name + ".sha256").write_text(f"{digest(archive)}  {archive.name}\n", encoding="utf-8", newline="\n")
        print(archive)


def manifest(args):
    import native_package

    version, revision = identity()
    required = [
        f"corvid-tui-{version}-{LINUX}.tar.gz",
        f"corvid-tui-{version}-{WINDOWS}.zip",
        f"corvid-desktop-{version}-{DESKTOP_LINUX}.tar.gz",
        f"corvid-desktop-{version}-{WINDOWS}.zip",
    ]
    assets = []
    for name in required:
        path = args.output / name
        checksum = path.with_name(name + ".sha256")
        if checksum.read_text().split()[0] != digest(path):
            raise ValueError(f"Package checksum mismatch: {name}")
        if name.endswith(".zip"):
            with zipfile.ZipFile(path) as archive:
                metadata = json.loads(archive.read("package.json"))
        else:
            with tarfile.open(path) as archive:
                metadata = json.load(archive.extractfile("package.json"))
        expected_name = f"corvid-{metadata['product']}-{metadata['version']}-{metadata['target']}"
        if metadata["version"] != version or metadata["source_revision"] != revision or not name.startswith(expected_name + "."):
            raise ValueError(f"Package identity mismatch: {name}")
    required.append(native_package.verify(args.output, version, revision))
    for name in ["install.sh", "install.ps1"]:
        copy(ROOT / "deploy" / name, args.output / name)
    for path in sorted(args.output.iterdir()):
        if path.name in required or path.name in ["install.sh", "install.ps1"]:
            assets.append({"name": path.name, "size": path.stat().st_size, "sha256": digest(path)})
    write_json(args.output / "release-manifest.json", {
        "schema_version": 1, "version": version, "source_revision": revision, "assets": assets,
    })


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    command = sub.add_parser("runtime")
    command.add_argument("--target", choices=[LINUX, WINDOWS], required=True)
    command.add_argument("--binary", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.set_defaults(run=runtime)
    command = sub.add_parser("package")
    command.add_argument("--product", choices=["tui", "desktop"], required=True)
    command.add_argument("--target", choices=[LINUX, WINDOWS, DESKTOP_LINUX], required=True)
    command.add_argument("--runtime", type=Path, required=True)
    command.add_argument("--linux-runtime", type=Path)
    command.add_argument("--windows-runtime", type=Path)
    command.add_argument("--build", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.set_defaults(run=package)
    command.add_argument("--without-agent-deck", action="store_true")
    command = sub.add_parser("manifest")
    command.add_argument("--output", type=Path, required=True)
    command.set_defaults(run=manifest)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
