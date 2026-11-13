#!/usr/bin/env python3
"""Validate the event diff and select the required CI jobs."""

import os
import re
import subprocess

LIGHTWEIGHT = "lightweight"
BUILD = "build"
PACKAGE = "package"
# Use the conservative GitHub path-filter limit for the main-push fallback.
PATH_FILTER_LIMIT = 300


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


def is_documentation(path):
    return (
        path in {"AGENTS.md", "README.md"}
        or (path.startswith("docs/") and path.endswith(".md"))
        or (
            path.startswith("docs/images/")
            and path.endswith((".jpg", ".jpeg", ".png", ".svg"))
        )
    )


def changed_paths(diff):
    # Each raw record has a metadata field and a path with rename detection off.
    fields = diff.decode("utf-8", errors="surrogateescape").split("\0")[:-1]
    for metadata, path in zip(fields[::2], fields[1::2], strict=True):
        old_mode, new_mode, *_ = metadata[1:].split()
        yield old_mode, new_mode, path


def docs_change(old_mode, new_mode, path):
    regular_or_absent = {"100644", "000000"}
    return (
        old_mode in regular_or_absent
        and new_mode in regular_or_absent
        and is_documentation(path)
    )


def rust_source_edit(old_mode, new_mode, path):
    return (
        old_mode == new_mode == "100644"
        and path.startswith(("crates/", "plugins/", "spikes/"))
        and path.endswith(".rs")
        and path.rsplit("/", 1)[-1] != "build.rs"
    )


def push_fallback(changes):
    # The native filter may truncate the file list or see only a rename's new
    # name. It also cannot distinguish documentation from a mode change.
    non_docs = [change for change in changes if not docs_change(*change)]
    if not non_docs:
        return False
    if len(changes) >= PATH_FILTER_LIMIT or any(
        is_documentation(path) for _, _, path in non_docs
    ):
        return True
    return (
        any(new == "000000" for _, new, _ in non_docs)
        and any(old == "000000" and is_documentation(path) for old, _, path in changes)
    )


def classify(event, ref, before):
    """Return the work to run in the Checks workflow for this event."""
    try:
        base = comparison_base(event, ref, before)
        if base is None:
            print("No comparison for this event. Running package integration.")
            return PACKAGE
        # Treat moves as deletions and additions so both paths are considered.
        diff = subprocess.check_output(
            ["git", "diff", "--raw", "--no-renames", "-z", base, "HEAD", "--"]
        )
    except subprocess.CalledProcessError:
        print("Could not compare the event commits. Running package integration.")
        return PACKAGE

    subprocess.run(["git", "diff", "--check", base, "HEAD", "--"], check=True)
    changes = list(changed_paths(diff))
    if not changes:
        return PACKAGE
    if event == "push":
        return PACKAGE if push_fallback(changes) else LIGHTWEIGHT

    source = False
    for old_mode, new_mode, path in changes:
        if docs_change(old_mode, new_mode, path):
            continue
        if rust_source_edit(old_mode, new_mode, path):
            source = True
            continue
        return PACKAGE
    return BUILD if source else LIGHTWEIGHT


if __name__ == "__main__":
    level = classify(
        os.environ["GITHUB_EVENT_NAME"],
        os.environ["GITHUB_REF"],
        os.environ.get("PUSH_BEFORE", ""),
    )
    print(f"Checks workflow selection: {level}")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        print(f"level={level}", file=output)
