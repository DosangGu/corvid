#!/usr/bin/env python3
"""Validate the event diff and select PR integration or main-push fallback packaging."""

import os
import re
import subprocess

# Use the conservative GitHub path-filter limit for the main-push fallback.
PATH_FILTER_LIMIT = 300
# Mirror the main-push paths in .github/workflows/package.yml.
PACKAGE_FILES = {
    ".gitattributes",
    ".gitignore",
    ".github/workflows/native.yml",
    ".github/workflows/package.yml",
    ".github/workflows/release.yml",
    "Cargo.lock",
    "Cargo.toml",
    "LICENSE",
    "rust-toolchain.toml",
}
PACKAGE_DIRECTORIES = (".cargo/", "assets/", "crates/", "deploy/", "plugins/", "spikes/")

PR_PACKAGE_FILES = {
    ".gitattributes",
    ".github/workflows/checks.yml",
    ".github/workflows/native.yml",
    ".github/workflows/package.yml",
    ".github/workflows/release.yml",
    "Cargo.lock",
    "LICENSE",
    "crates/corvid-client/src/endpoint/install_windows.ps1",
    "crates/corvid-client/src/upgrade.rs",
    "crates/corvid-daemon/src/upgrade.rs",
    "crates/corvid-desktop/src/workspace_view/update.rs",
    "crates/corvid-desktop/src/workspace_view/update/state.rs",
    "crates/corvid-protocol/src/upgrade.rs",
    "crates/corvid-tui/src/update.rs",
    "deploy/install.ps1",
    "deploy/install.sh",
    "deploy/macos_dock_fixture.swift",
    "deploy/release-notes.md",
    "deploy/release.py",
    "deploy/test_icons.py",
    "deploy/test_install.py",
    "deploy/test_macos.py",
    "deploy/test_macos_dock_update.py",
    "deploy/test_native.py",
}
PR_PACKAGE_PREFIXES = (
    ".cargo/",
    "assets/",
    "crates/corvid-client/src/endpoint/bootstrap",
    "crates/corvid-update/",
    "deploy/cargo-desktop/",
    "deploy/linux/",
    "deploy/macos/",
    "deploy/systemd/",
    "deploy/windows/",
)


def comparison_base(event, ref, before):
    if event == "pull_request":
        # Actions checks out the synthetic merge commit, whose first parent is
        # the base tree. This measures the changes CI is about to test.
        parents = subprocess.check_output(
            ["git", "show", "-s", "--format=%P", "HEAD"], text=True
        ).split()
        if len(parents) == 2:
            return parents[0]
    if event == "push" and ref == "refs/heads/main":
        if re.fullmatch(r"[0-9a-f]{40}", before) and before != "0" * 40:
            return before
    return None


def changed_paths(diff):
    # Each raw record has a metadata field and a path with rename detection off.
    fields = diff.decode("utf-8", errors="surrogateescape").split("\0")[:-1]
    for metadata, path in zip(fields[::2], fields[1::2], strict=True):
        old_mode, new_mode, *_ = metadata[1:].split()
        yield old_mode, new_mode, path


def is_build_input(path):
    return path in PACKAGE_FILES or path.startswith(PACKAGE_DIRECTORIES)


def is_pr_packaging_input(path):
    return (
        path in PR_PACKAGE_FILES
        or path.startswith(PR_PACKAGE_PREFIXES)
        or re.fullmatch(r"deploy/[^/]*package[^/]*\.py", path) is not None
    )


def push_fallback(changes):
    # GitHub's path filter may truncate the file list or see only a rename's new
    # name. Keep the fallback scoped to the same inputs as the path filter.
    package_inputs = [
        (old, new) for old, new, path in changes if is_build_input(path)
    ]
    if not package_inputs:
        return False
    if len(changes) >= PATH_FILTER_LIMIT:
        return True
    return (
        any(new == "000000" for _, new in package_inputs)
        and any(
            old == "000000" and not is_build_input(path) for old, _, path in changes
        )
    )


def validate_changes(event, ref, before):
    """Return the integration job to run after validation."""
    pull_request = event == "pull_request"
    main_push = event == "push" and ref == "refs/heads/main"
    fallback = "package" if pull_request or main_push else "none"
    try:
        base = comparison_base(event, ref, before)
        if base is None:
            print("No comparison for this event.")
            return fallback
        # Treat moves as deletions and additions so both paths are considered.
        diff = subprocess.check_output(
            ["git", "diff", "--raw", "--no-renames", "-z", base, "HEAD", "--"]
        )
    except subprocess.CalledProcessError:
        print("Could not compare the event commits.")
        return fallback

    subprocess.run(["git", "diff", "--check", base, "HEAD", "--"], check=True)
    changes = list(changed_paths(diff))
    if pull_request:
        paths = [path for _, _, path in changes]
        if any(map(is_pr_packaging_input, paths)):
            return "package"
        if any(map(is_build_input, paths)):
            return "native"
    if main_push and push_fallback(changes):
        return "package"
    return "none"


if __name__ == "__main__":
    integration = validate_changes(
        os.environ["GITHUB_EVENT_NAME"],
        os.environ["GITHUB_REF"],
        os.environ.get("PUSH_BEFORE", ""),
    )
    print(f"Integration to run: {integration}")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        print(f"integration={integration}", file=output)
