#!/usr/bin/env python3
"""Validate a release ref and publish a fully assembled draft."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent
REPOSITORY = "DosangGu/corvid"


def validate():
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?", version):
        raise ValueError("Invalid workspace release version")
    ref = os.environ.get("GITHUB_REF", "")
    if ref.startswith("refs/tags/") and ref != f"refs/tags/v{version}":
        raise ValueError("Tag and workspace version differ")
    return version


def gh(*arguments, **kwargs):
    return subprocess.run(["gh", *arguments, "--repo", REPOSITORY], text=True, **kwargs)


def publish(directory):
    version = validate()
    tag = f"v{version}"
    if os.environ.get("GITHUB_REF") != f"refs/tags/{tag}":
        raise ValueError("Publication requires the matching release tag")
    manifest = json.loads((directory / "release-manifest.json").read_text())
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if manifest["version"] != version or manifest["source_revision"] != revision:
        raise ValueError("Release identity mismatch")
    for asset in manifest["assets"]:
        path = directory / asset["name"]
        if path.parent != directory or path.stat().st_size != asset["size"]:
            raise ValueError("Invalid release asset")
        with path.open("rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != asset["sha256"]:
                raise ValueError("Release checksum mismatch")
    existing = gh("release", "view", tag, "--json", "isDraft", capture_output=True)
    if existing.returncode == 0:
        if not json.loads(existing.stdout)["isDraft"]:
            raise ValueError("This release is already published; create a new version")
    else:
        gh("release", "create", tag, "--verify-tag", "--draft", "--title", f"Corvid {tag}",
           "--notes-file", str(ROOT / "deploy/release-notes.md"), check=True)
    files = sorted(str(path) for path in directory.iterdir() if path.is_file())
    gh("release", "upload", tag, *files, "--clobber", check=True)
    uploaded = gh("release", "view", tag, "--json", "assets", check=True, capture_output=True)
    actual = {asset["name"]: asset["size"] for asset in json.loads(uploaded.stdout)["assets"]}
    expected = {Path(path).name: Path(path).stat().st_size for path in files}
    if actual != expected:
        raise ValueError("Draft asset inventory does not match the release")
    options = ["--draft=false", "--prerelease=" + str("-" in version).lower()]
    if "-" not in version:
        options.append("--latest")
    gh("release", "edit", tag, *options, check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["validate", "publish"])
    parser.add_argument("--assets", type=Path)
    args = parser.parse_args()
    if args.command == "publish":
        if args.assets is None:
            parser.error("publish requires --assets")
        publish(args.assets)
    else:
        print(f"Validated Corvid {validate()}")
