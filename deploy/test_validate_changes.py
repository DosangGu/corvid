#!/usr/bin/env python3
"""Exercise the CI change classifier against real Git diffs."""

from contextlib import chdir, redirect_stdout
from io import StringIO
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest

from validate_changes import (
    BUILD,
    LIGHTWEIGHT,
    PACKAGE,
    PACKAGE_DIRECTORIES,
    PACKAGE_FILES,
    PATH_FILTER_LIMIT,
    classify,
)

WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/package.yml"


class PackagePathsTests(unittest.TestCase):
    def test_push_fallback_uses_the_workflow_paths(self):
        block = WORKFLOW.read_text(encoding="utf-8").split("    paths:\n", 1)[1]
        workflow_paths = {
            line.removeprefix("      - ").strip().strip("'\"")
            for line in block.split("\n\n", 1)[0].splitlines()
            if line.startswith("      - ")
        }
        self.assertSetEqual(
            workflow_paths,
            PACKAGE_FILES | {f"{path}**" for path in PACKAGE_DIRECTORIES},
        )


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
        return classify("push", "refs/heads/main", self.base)

    def check_pr(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.git("reset", "--hard", self.base)
        self.git("merge", "--quiet", "--no-ff", "main")
        level = classify("pull_request", "refs/pull/1/merge", "")
        self.git("switch", "--quiet", "main")
        self.git("branch", "-D", "topic")
        return level

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
        self.assertEqual(classify("pull_request", "refs/pull/1/merge", ""), LIGHTWEIGHT)

    def test_docs_only_push(self):
        self.write("README.md", "# Documentation update\n")
        self.commit()
        self.assertEqual(self.check_push(), LIGHTWEIGHT)

    def test_build_inputs_and_unknown_paths_require_package_integration(self):
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
                self.assertEqual(self.check_pr(), PACKAGE)
                # Main delegates packaging inputs to its path-filtered workflow.
                self.assertEqual(self.check_push(), LIGHTWEIGHT)

    def test_comment_only_source_and_docs_still_run_build_checks(self):
        self.write("crates/example/src/lib.rs", "// A comment\npub fn example() {}\n")
        self.commit()
        self.write("README.md", "# Documentation update\n")
        self.commit()
        self.assertEqual(self.check_pr(), BUILD)
        self.assertEqual(self.check_push(), LIGHTWEIGHT)

    def test_packaging_input_takes_precedence_over_source_and_docs(self):
        self.write("crates/example/src/lib.rs", "pub fn changed() {}\n")
        self.write("README.md", "# Documentation update\n")
        self.write("deploy/install.sh", "exit 0\n")
        self.commit()
        self.assertEqual(self.check_pr(), PACKAGE)

    def test_new_rust_path_requires_package_integration(self):
        self.write("crates/example/src/new.rs", "pub fn new() {}\n")
        self.commit()
        self.assertEqual(self.check_pr(), PACKAGE)

    def test_move_from_source_to_docs_checks_the_deleted_path(self):
        Path("docs").mkdir()
        self.git("mv", "crates/example/src/lib.rs", "docs/example.md")
        self.commit()
        self.assertEqual(self.check_pr(), PACKAGE)
        self.assertEqual(self.check_push(), PACKAGE)

    def test_executable_markdown_gets_package_integration_on_prs(self):
        self.git("update-index", "--chmod=+x", "README.md")
        self.git("commit", "--quiet", "-m", "Executable documentation fixture")
        self.assertEqual(self.check_pr(), PACKAGE)
        self.assertEqual(self.check_push(), LIGHTWEIGHT)

    def test_large_push_falls_back_to_package_integration(self):
        for index in range(PATH_FILTER_LIMIT):
            self.write(f"docs/{index:03}.md", "# Guide\n")
        self.write("docs/unknown.py", "# Unrelated to packaging\n")
        self.commit()
        self.assertEqual(self.check_push(), LIGHTWEIGHT)
        self.write("crates/example/src/lib.rs", "pub fn changed() {}\n")
        self.commit()
        self.assertEqual(self.check_push(), PACKAGE)

    def test_full_pipeline_when_no_reliable_diff_is_available(self):
        for event, ref, before in (
            ("workflow_dispatch", "refs/heads/main", ""),
            ("push", "refs/tags/v0.1.3", self.base),
            ("push", "refs/heads/main", "0" * 40),
            ("pull_request", "refs/pull/1/merge", ""),
        ):
            with self.subTest(event=event, ref=ref, before=before):
                self.assertEqual(classify(event, ref, before), PACKAGE)

    def test_empty_push_diff_stays_lightweight(self):
        self.assertEqual(self.check_push(), LIGHTWEIGHT)

    def test_missing_push_commit_requires_the_full_pipeline(self):
        self.assertEqual(classify("push", "refs/heads/main", "a" * 40), PACKAGE)

    def test_whitespace_check_covers_committed_changes(self):
        self.write("README.md", "# Trailing space \n")
        self.commit()
        with self.assertRaises(subprocess.CalledProcessError):
            self.check_push()


if __name__ == "__main__":
    unittest.main()
