#!/usr/bin/env python3
"""Exercise transient and permanent DMG creation failures without macOS tools."""

from contextlib import redirect_stderr
from io import StringIO
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from macos_package import create_dmg


class CreateDmgTests(unittest.TestCase):
    def setUp(self):
        self.directory = Path(self.enterContext(TemporaryDirectory()))
        self.volume = self.directory / "volume"
        self.volume.mkdir()
        self.artifact = self.directory / "Corvid.dmg"
        self.stderr = self.enterContext(redirect_stderr(StringIO()))
        self.sleep = self.enterContext(patch("macos_package.time.sleep"))

    def run_create(self, errors):
        def run(command, **kwargs):
            self.assertEqual(Path(command[-1]), self.artifact)
            self.assertEqual(kwargs["env"]["LC_ALL"], "C")
            self.assertFalse(self.artifact.exists(), "Retry must remove the partial image")
            error = next(errors)
            self.artifact.write_bytes(b"partial image" if error else b"complete image")
            return subprocess.CompletedProcess(command, 1 if error else 0, stderr=error)

        return self.enterContext(patch("macos_package.subprocess.run", side_effect=run))

    def test_success_keeps_the_image_without_retrying(self):
        run = self.run_create(iter([""]))
        create_dmg(self.volume, self.artifact)
        self.assertEqual(run.call_count, 1)
        self.sleep.assert_not_called()
        self.assertEqual(self.artifact.read_bytes(), b"complete image")

    def test_resource_busy_retries_with_backoff_and_removes_partial_images(self):
        error = "hdiutil: create failed - Resource busy\n"
        run = self.run_create(iter([error, error, ""]))
        create_dmg(self.volume, self.artifact)
        self.assertEqual(run.call_count, 3)
        self.assertEqual([call.args[0] for call in self.sleep.call_args_list], [5, 10])
        self.assertEqual(self.artifact.read_bytes(), b"complete image")
        self.assertEqual(self.stderr.getvalue().count(error), 2)

    def test_persistent_resource_busy_fails_after_three_attempts(self):
        error = "hdiutil: create failed - Resource busy\n"
        run = self.run_create(iter([error] * 3))
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            create_dmg(self.volume, self.artifact)
        self.assertEqual(run.call_count, 3)
        self.assertEqual(self.sleep.call_count, 2)
        self.assertEqual(failure.exception.stderr, error)

    def test_other_failures_are_reported_without_retrying(self):
        error = "hdiutil: create failed - No space left on device\n"
        run = self.run_create(iter([error]))
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            create_dmg(self.volume, self.artifact)
        self.assertEqual(run.call_count, 1)
        self.sleep.assert_not_called()
        self.assertEqual(failure.exception.stderr, error)
        self.assertEqual(self.stderr.getvalue(), error)


if __name__ == "__main__":
    unittest.main()
