"""Compare clean protox and reference protoc builds and run the framing fixtures."""

import argparse
from hashlib import file_digest, sha256
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import tomllib
from zipfile import ZipFile


PROTOC_SHA256 = "c4bc672d9d49214dc8cafdceadf4df92182d6ca8e3ec65a56b2d7de5602669b4"
REFERENCE_PROTOC = "CORVID_SCHEMA_REFERENCE_PROTOC"
RUST_OUTPUTS = ("corvid.protocol.v1.rs", "wire_preflight.rs")


def verify_tools(root):
    with (root / "rust-toolchain.toml").open("rb") as manifest:
        expected_rust = tomllib.load(manifest)["toolchain"]["channel"]
    actual_rust = subprocess.check_output(["rustc", "--version"], cwd=root, text=True).split()[1]
    if actual_rust != expected_rust:
        raise RuntimeError(f"Use Rust {expected_rust}")


def extract_protoc(archive, compiler):
    with archive.open("rb") as source:
        digest = file_digest(source, "sha256").hexdigest()
        if digest != PROTOC_SHA256:
            raise RuntimeError("Expected the official protoc 36.1 Linux x86_64 archive")
        source.seek(0)
        with ZipFile(source) as release:
            compiler.write_bytes(release.read("bin/protoc"))
    compiler.chmod(0o700)


def copy_tracked_sources(root, destination, paths):
    for path in paths:
        source = root / path
        if source.is_file():
            target = destination / path
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)


def cargo_messages(source, env, *args):
    result = subprocess.run(
        ["cargo", *args, "--locked", "--message-format=json-render-diagnostics"],
        cwd=source, env=env, check=True, text=True, stdout=subprocess.PIPE,
    )
    return [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]


def build(source, reference_compiler=None):
    target = source / "target"
    env = os.environ | {"PROTOC": str(source / "missing-protoc"), "CARGO_TARGET_DIR": str(target)}
    env.pop("CARGO_BUILD_TARGET", None)
    env.pop(REFERENCE_PROTOC, None)
    if reference_compiler is not None:
        env[REFERENCE_PROTOC] = str(reference_compiler)
    build_messages = cargo_messages(source, env, "build", "-p", "corvid-tui")
    test_messages = cargo_messages(
        source, env, "test", "-p", "corvid-protocol", "--test", "framing", "--no-run",
    )
    fixture, = [
        Path(artifact["executable"]) for artifact in test_messages
        if artifact["reason"] == "compiler-artifact"
        and artifact["target"]["kind"] == ["test"]
        and artifact["target"]["name"] == "framing"
        and artifact.get("executable")
    ]
    return fixture, generated(build_messages + test_messages, source)


def generated(messages, source):
    protocol_manifest = str(source / "crates/corvid-protocol/Cargo.toml")
    protocol_id, = {
        message["package_id"]
        for message in messages
        if message["reason"] == "compiler-artifact" and message["manifest_path"] == protocol_manifest
    }
    out_dirs = {
        Path(message["out_dir"])
        for message in messages
        if message["reason"] == "build-script-executed" and message["package_id"] == protocol_id
    }
    if not out_dirs:
        raise RuntimeError("Cargo did not report a corvid-protocol output directory")

    output = {}
    for name in RUST_OUTPUTS:
        variants = {(directory / name).read_bytes() for directory in out_dirs}
        if len(variants) != 1:
            raise RuntimeError(f"Rust build variants disagree on generated {name}")
        output[name], = variants
    return output


def test_without_compiler(source, fixture, empty_path):
    env = os.environ | {"PATH": str(empty_path), "PROTOC": str(empty_path / "protoc")}
    subprocess.run([source / "target/debug/corvid", "--version"], env=env, check=True)
    subprocess.run([fixture], env=env, check=True)


def verify(root, archive):
    verify_tools(root)
    paths = subprocess.check_output(["git", "ls-files", "-z"], cwd=root).split(b"\0")
    paths = [Path(os.fsdecode(path)) for path in paths if path]
    with tempfile.TemporaryDirectory(prefix="corvid-schema-") as temporary:
        directory = Path(temporary)
        compiler = directory / "protoc"
        extract_protoc(archive, compiler)
        first, second = directory / "first", directory / "second"
        for source in (first, second):
            copy_tracked_sources(root, source, paths)
        fixture, first_output = build(first)
        _, second_output = build(second, compiler)
        for name, contents in first_output.items():
            if contents != second_output[name]:
                raise RuntimeError(f"protox and reference protoc builds disagree on generated {name}")
            print(f"{sha256(contents).hexdigest()}  {name}")

        empty_path = directory / "runtime-path"
        empty_path.mkdir()
        test_without_compiler(first, fixture, empty_path)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path, help="official protoc-36.1-linux-x86_64.zip")
    args = parser.parse_args()
    if sys.platform != "linux" or platform.machine() != "x86_64":
        parser.error("this verification fixture requires Linux x86_64")
    verify(Path(__file__).resolve().parents[3], args.archive)
