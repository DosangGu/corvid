#!/usr/bin/env python3
"""Exercise the CI change classifier against real Git diffs."""

from contextlib import chdir, redirect_stdout
from io import StringIO
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest

from validate_changes import requires_full_pipeline


class ValidateChangesTests(unittest.TestCase):
    def setUp(self):
        self.enterContext(chdir(self.enterContext(TemporaryDirectory())))
        self.enterContext(redirect_stdout(StringIO()))
        self.git("init", "--initial-branch=main")
        self.git("config", "user.name", "CI fixture")
        self.git("config", "user.email", "ci@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.write("README.md", "# Corvid\n")
        self.write("crates/example/src/lib.rs", "pub fn example() {}\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(["git", *args], text=True).strip()

    def write(self, path, contents):
        file = Path(path)
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(contents, encoding="utf-8")

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "--quiet", "-m", "Fixture")
        return self.git("rev-parse", "HEAD")

    def check_push(self):
        return requires_full_pipeline("push", "refs/heads/main", self.base)

    def test_docs_pr_uses_the_checked_out_merge_base_tree(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write("AGENTS.md", "# Guidelines\n")
        self.write("docs/guide with spaces.md", "# Guide\n")
        self.write("docs/images/example.svg", "<svg/>\n")
        self.commit()
        self.git("switch", "--quiet", "main")
        self.write("Cargo.lock", "# Base branch change\n")
        self.commit()
        self.git("merge", "--quiet", "--no-ff", "topic")
        self.assertFalse(requires_full_pipeline("pull_request", "refs/pull/1/merge", ""))

    def test_docs_only_push(self):
        self.write("README.md", "# Documentation update\n")
        self.commit()
        self.assertFalse(self.check_push())

    def test_build_inputs_and_unknown_paths_require_the_full_pipeline(self):
        for path in (
            "Cargo.lock",
            "crates/example/build.rs",
            "crates/corvid-protocol/proto/example.proto",
            "deploy/release-notes.md",
            "deploy/windows/CorvidSetup.iss",
            "assets/icons/corvid.png",
            ".github/workflows/package.yml",
            "docs/unknown.py",
        ):
            with self.subTest(path=path):
                self.git("reset", "--hard", self.base)
                self.write(path, "Fixture\n")
                self.commit()
                self.assertTrue(self.check_push())

    def test_mixed_push_covers_earlier_commits_and_comment_only_edits(self):
        self.write("crates/example/src/lib.rs", "// A comment\npub fn example() {}\n")
        self.commit()
        self.write("README.md", "# Documentation update\n")
        self.commit()
        self.assertTrue(self.check_push())

    def test_move_from_source_to_docs_checks_the_deleted_path(self):
        Path("docs").mkdir()
        self.git("mv", "crates/example/src/lib.rs", "docs/example.md")
        self.commit()
        self.assertTrue(self.check_push())

    def test_executable_markdown_requires_the_full_pipeline(self):
        self.git("update-index", "--chmod=+x", "README.md")
        self.git("commit", "--quiet", "-m", "Executable documentation fixture")
        self.assertTrue(self.check_push())

    def test_full_pipeline_when_no_reliable_diff_is_available(self):
        for event, ref, before in (
            ("workflow_dispatch", "refs/heads/main", ""),
            ("push", "refs/tags/v0.1.3", self.base),
            ("push", "refs/heads/main", "0" * 40),
            ("push", "refs/heads/main", self.base),
            ("pull_request", "refs/pull/1/merge", ""),
        ):
            with self.subTest(event=event, ref=ref, before=before):
                self.assertTrue(requires_full_pipeline(event, ref, before))

    def test_missing_push_commit_requires_the_full_pipeline(self):
        self.assertTrue(requires_full_pipeline("push", "refs/heads/main", "a" * 40))

    def test_whitespace_check_covers_committed_changes(self):
        self.write("README.md", "# Trailing space \n")
        self.commit()
        with self.assertRaises(subprocess.CalledProcessError):
            self.check_push()


if __name__ == "__main__":
    unittest.main()
