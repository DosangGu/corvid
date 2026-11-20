#!/usr/bin/env python3
"""Check workflow routing and validate changes against real Git diffs."""

from contextlib import chdir, redirect_stdout
from io import StringIO
from pathlib import Path
import re
import subprocess
from tempfile import TemporaryDirectory
import unittest

from package import DESKTOP_TARGETS, RUNTIME_TARGETS
from validate_changes import (
    PACKAGE_DIRECTORIES,
    PACKAGE_FILES,
    PATH_FILTER_LIMIT,
    validate_changes,
)

WORKFLOWS = Path(__file__).resolve().parents[1] / ".github/workflows"


def workflow_patterns(block):
    return {
        line.removeprefix("      - ").strip().strip("'\"")
        for line in block.split("\n\n", 1)[0].splitlines()
        if line.startswith("      - ")
    }


class WorkflowsTests(unittest.TestCase):
    def test_checks_owns_pr_trigger_for_native_and_package(self):
        for filename in ("native.yml", "package.yml"):
            with self.subTest(filename=filename):
                text = (WORKFLOWS / filename).read_text(encoding="utf-8")
                self.assertNotIn("\n  pull_request:", text)
                checks = (WORKFLOWS / "checks.yml").read_text(encoding="utf-8")
                self.assertIn(f"uses: ./.github/workflows/{filename}", checks)

    def test_push_fallback_uses_the_workflow_paths(self):
        push = (WORKFLOWS / "package.yml").read_text(encoding="utf-8").split("  push:\n", 1)[1]
        block = push.split("    paths:\n", 1)[1]
        self.assertSetEqual(
            workflow_patterns(block),
            PACKAGE_FILES | {f"{path}**" for path in PACKAGE_DIRECTORIES},
        )

    def test_packaging_uses_the_same_targets_as_the_native_builds(self):
        text = (WORKFLOWS / "native.yml").read_text(encoding="utf-8")
        builds = re.findall(r"- runner: (\S+)\s+runtime: (\S+)\s+desktop: (\S+)", text)

        text = (WORKFLOWS / "package.yml").read_text(encoding="utf-8")
        installers = re.findall(r"- os: (\S+)\s+target: (\S+)\s+native: (\S+)", text)
        self.assertCountEqual(installers, [(runner, desktop, runtime) for runner, runtime, desktop in builds])
        self.assertCountEqual((runtime for _, runtime, _ in builds), RUNTIME_TARGETS)
        self.assertCountEqual((desktop for _, _, desktop in builds), DESKTOP_TARGETS)


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
        return validate_changes("push", "refs/heads/main", self.base)

    def check_pr(self):
        self.git("switch", "--quiet", "main")
        self.git("merge", "--quiet", "--no-ff", "topic")
        return validate_changes("pull_request", "refs/pull/1/merge", "")

    def test_pr_whitespace_check_uses_the_checked_out_merge_base_tree(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write("docs/guide with spaces.md", "# Guide\n")
        self.commit()
        self.git("switch", "--quiet", "main")
        self.write("Cargo.lock", "# Trailing space on base branch \n")
        self.commit()
        self.assertEqual(self.check_pr(), "none")

    def test_pr_whitespace_check_covers_committed_changes(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write("README.md", "# Trailing space in PR \n")
        self.commit()
        with self.assertRaises(subprocess.CalledProcessError):
            self.check_pr()

    def test_pr_with_only_docs_and_images_stays_lightweight(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write("README.md", "# Documentation update\n")
        self.write("docs/images/desktop.svg", "<svg></svg>\n")
        self.commit()
        self.assertEqual(self.check_pr(), "none")

    def test_dependabot_config_pr_stays_lightweight(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write(".github/dependabot.yml", "version: 2\n")
        self.commit()
        self.assertEqual(self.check_pr(), "none")

    def test_dependabot_config_with_workspace_manifest_runs_native_checks(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write(".github/dependabot.yml", "version: 2\n")
        self.write("Cargo.toml", "[workspace]\n")
        self.commit()
        self.assertEqual(self.check_pr(), "native")

    def test_ordinary_source_pr_runs_native_checks(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write("crates/example/src/lib.rs", "pub fn changed() {}\n")
        self.write("deploy/example/package.py", "# Unrelated nested script\n")
        self.commit()
        self.assertEqual(self.check_pr(), "native")

    def test_pr_with_cargo_lock_runs_packaging(self):
        self.git("switch", "--quiet", "-c", "topic")
        self.write("crates/example/src/lib.rs", "pub fn changed() {}\n")
        self.write("Cargo.lock", "# Dependencies changed\n")
        self.commit()
        self.assertEqual(self.check_pr(), "package")

    def test_pr_removing_a_packaging_input_runs_packaging(self):
        self.write("deploy/test_package.py", "# Packaging fixture\n")
        self.commit()
        self.git("switch", "--quiet", "-c", "topic")
        self.git("mv", "deploy/test_package.py", "packaging-notes.md")
        self.commit()
        self.assertEqual(self.check_pr(), "package")

    def test_docs_only_push(self):
        self.write("README.md", "# Documentation update\n")
        self.commit()
        self.assertEqual(self.check_push(), "none")

    def test_ordinary_packaging_input_push_uses_the_package_workflow(self):
        self.write("Cargo.lock", "# Dependencies changed\n")
        self.commit()
        self.assertEqual(self.check_push(), "none")

    def test_move_from_source_to_docs_checks_the_deleted_path(self):
        Path("docs").mkdir()
        self.git("mv", "crates/example/src/lib.rs", "docs/example.md")
        self.commit()
        self.assertEqual(self.check_push(), "package")

    def test_large_push_falls_back_to_package_integration(self):
        for index in range(PATH_FILTER_LIMIT):
            self.write(f"docs/{index:03}.md", "# Guide\n")
        self.write("docs/unknown.py", "# Unrelated to packaging\n")
        self.commit()
        self.assertEqual(self.check_push(), "none")
        self.write("crates/example/src/lib.rs", "pub fn changed() {}\n")
        self.commit()
        self.assertEqual(self.check_push(), "package")

    def test_push_without_a_reliable_diff_runs_package_integration(self):
        self.assertEqual(validate_changes("push", "refs/heads/main", "0" * 40), "package")

    def test_pr_without_a_merge_commit_requires_the_full_pipeline(self):
        self.assertEqual(validate_changes("pull_request", "refs/pull/1/merge", ""), "package")

    def test_empty_push_diff_stays_lightweight(self):
        self.assertEqual(self.check_push(), "none")

    def test_missing_push_commit_requires_the_full_pipeline(self):
        self.assertEqual(validate_changes("push", "refs/heads/main", "a" * 40), "package")

    def test_whitespace_check_covers_committed_changes(self):
        self.write("README.md", "# Trailing space \n")
        self.commit()
        with self.assertRaises(subprocess.CalledProcessError):
            self.check_push()


if __name__ == "__main__":
    unittest.main()
