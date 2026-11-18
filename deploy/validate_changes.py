#!/usr/bin/env python3
"""Validate the event diff and detect main pushes that need a packaging fallback."""

import os
import re
import subprocess

# Use the conservative GitHub path-filter limit for the main-push fallback.
PATH_FILTER_LIMIT = 300
# Mirror the main-push paths in .github/workflows/package.yml.
PACKAGE_FILES = {
    ".gitattributes",
    ".gitignore",
    ".github/workflows/linux.yml",
    ".github/workflows/macos.yml",
    ".github/workflows/package.yml",
    ".github/workflows/release.yml",
    ".github/workflows/windows.yml",
    "Cargo.lock",
    "Cargo.toml",
    "LICENSE",
    "rust-toolchain.toml",
}
PACKAGE_DIRECTORIES = (".cargo/", "assets/", "crates/", "deploy/", "plugins/", "spikes/")


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


def is_packaging_input(path):
    return path in PACKAGE_FILES or path.startswith(PACKAGE_DIRECTORIES)


def push_fallback(changes):
    # The native filter may truncate the file list or see only a rename's new
    # name. Keep the fallback scoped to the same inputs as the native filter.
    package_inputs = [
        (old, new) for old, new, path in changes if is_packaging_input(path)
    ]
    if not package_inputs:
        return False
    if len(changes) >= PATH_FILTER_LIMIT:
        return True
    return (
        any(new == "000000" for _, new in package_inputs)
        and any(
            old == "000000" and not is_packaging_input(path) for old, _, path in changes
        )
    )


def validate_changes(event, ref, before):
    """Return whether the Checks workflow should run fallback packaging."""
    main_push = event == "push" and ref == "refs/heads/main"
    try:
        base = comparison_base(event, ref, before)
        if base is None:
            print("No comparison for this event.")
            return main_push
        # Treat moves as deletions and additions so both paths are considered.
        diff = subprocess.check_output(
            ["git", "diff", "--raw", "--no-renames", "-z", base, "HEAD", "--"]
        )
    except subprocess.CalledProcessError:
        print("Could not compare the event commits.")
        return main_push

    subprocess.run(["git", "diff", "--check", base, "HEAD", "--"], check=True)
    return main_push and push_fallback(list(changed_paths(diff)))


if __name__ == "__main__":
    fallback = validate_changes(
        os.environ["GITHUB_EVENT_NAME"],
        os.environ["GITHUB_REF"],
        os.environ.get("PUSH_BEFORE", ""),
    )
    value = "true" if fallback else "false"
    print(f"Run fallback packaging: {value}")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        print(f"package_fallback={value}", file=output)
