#!/usr/bin/env python3
"""Validate the event diff and decide whether to run builds and packaging."""

import os
import re
import subprocess


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


def docs_only(diff):
    # Each raw record has a metadata field and a path with rename detection off.
    fields = diff.decode("utf-8", errors="surrogateescape").split("\0")[:-1]
    regular_or_absent = {"100644", "000000"}
    for metadata, path in zip(fields[::2], fields[1::2], strict=True):
        old_mode, new_mode, *_ = metadata[1:].split()
        if old_mode not in regular_or_absent or new_mode not in regular_or_absent:
            return False
        if not is_documentation(path):
            return False
    return bool(fields)


def requires_full_pipeline(event, ref, before):
    try:
        base = comparison_base(event, ref, before)
        if base is None:
            print("No comparison for this event. Running the full pipeline.")
            return True
        # Treat moves as deletions and additions so both paths are considered.
        diff = subprocess.check_output(
            ["git", "diff", "--raw", "--no-renames", "-z", base, "HEAD", "--"]
        )
    except subprocess.CalledProcessError:
        print("Could not compare the event commits. Running the full pipeline.")
        return True

    subprocess.run(["git", "diff", "--check", base, "HEAD", "--"], check=True)
    if docs_only(diff):
        print("Only documentation changed. Skipping builds and packaging.")
        return False
    print("Running the full pipeline for these changes.")
    return True


if __name__ == "__main__":
    full_pipeline = requires_full_pipeline(
        os.environ["GITHUB_EVENT_NAME"],
        os.environ["GITHUB_REF"],
        os.environ.get("PUSH_BEFORE", ""),
    )
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        print(f"full_pipeline={str(full_pipeline).lower()}", file=output)
