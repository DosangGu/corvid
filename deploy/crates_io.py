#!/usr/bin/env python3
"""Verify or publish the Corvid TUI source packages."""

import argparse
import json
import os
from pathlib import Path
import subprocess
from urllib.error import HTTPError
from urllib.request import urlopen

from release import ROOT, validate


def package_arguments(names):
    return [argument for name in names for argument in ("-p", name)]


def verify(names, version, target):
    # The TUI depends on Agent Deck's library with its UI feature disabled.
    subprocess.run(
        ["cargo", "package", "--locked", "--registry", "crates-io",
         "--no-default-features", *package_arguments(names)],
        cwd=ROOT, check=True,
    )
    binary = target / "debug" / ("corvid.exe" if os.name == "nt" else "corvid")
    actual = subprocess.check_output([binary, "--version"], text=True, cwd=target).strip()
    if actual != f"corvid {version}":
        raise ValueError(f"Unexpected packaged binary version: {actual!r}")
    print(actual)


def is_published(name, version):
    url = f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}"
    try:
        with urlopen(url, timeout=30) as response:
            for line in response:
                entry = json.loads(line)
                if entry["vers"] == version:
                    if entry["yanked"]:
                        raise ValueError(f"{name} {version} is yanked on crates.io")
                    return True
    except HTTPError as error:
        if error.code != 404:
            raise
    return False


def publish(names, version):
    subprocess.run(
        ["git", "describe", "--exact-match", "--tags", "--match", f"v{version}", "HEAD"],
        cwd=ROOT, check=True,
    )
    # Cargo rejects an entire invocation if any selected version already exists.
    # On retry it can resolve the published dependencies from the registry.
    remaining = [name for name in names if not is_published(name, version)]
    if remaining:
        subprocess.run(
            ["cargo", "publish", "--locked", "--registry", "crates-io",
             "--no-default-features", *package_arguments(remaining)],
            cwd=ROOT, check=True,
        )
    else:
        print(f"All Corvid {version} crates are already published")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["verify", "publish"])
    args = parser.parse_args()

    version = validate()
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
        cwd=ROOT,
    ))
    names = sorted(package["name"] for package in metadata["packages"]
                   if package["publish"] == ["crates-io"])
    if "corvid-tui" not in names:
        raise ValueError("corvid-tui must be enabled for crates.io publication")
    if args.command == "verify":
        verify(names, version, Path(metadata["target_directory"]))
    else:
        publish(names, version)
