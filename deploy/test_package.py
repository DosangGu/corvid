#!/usr/bin/env python3
"""Check architecture isolation and complete release inventories with synthetic payloads."""
from contextlib import redirect_stdout
from io import StringIO
import json
from pathlib import Path
import tarfile
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import zipfile

import native_package
import package


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(TemporaryDirectory()))
        self.version = "1.2.3"
        self.revision = "a" * 40
        self.enterContext(patch("package.identity", return_value=(self.version, self.revision)))
        self.enterContext(redirect_stdout(StringIO()))
        self.runtimes = {}
        for target in package.RUNTIME_TARGETS:
            directory = self.root / target
            directory.mkdir()
            binary = directory / ("corvid.exe" if target in package.WINDOWS_TARGETS else "corvid")
            binary.write_bytes(target.encode())
            package.write_json(directory / "manifest.json", {
                "schema_version": 1, "target": target, "version": self.version,
                "source_revision": self.revision, "size": binary.stat().st_size,
                "sha256": package.digest(binary),
            })
            daemon = binary.with_name(binary.name.replace("corvid", "corvidd"))
            daemon.write_bytes(f"daemon:{target}".encode())
            package.record_runtime(daemon, directory / "daemon-manifest.json",
                                   target, self.version, self.revision)
            self.runtimes[target] = directory
        self.build = self.root / "build"
        self.build.mkdir()
        for name in ("corvid-desktop", "corvid-agent-deck", "corvid-launch",
                     "corvid-desktop-launch", "corvid-agent-deck-launch"):
            for suffix in ("", ".exe"):
                (self.build / (name + suffix)).write_bytes(b"synthetic executable")
        self.output = self.root / "dist"

    def arguments(self, product, target):
        return SimpleNamespace(
            product=product, target=target, build=self.build, output=self.output,
            runtime=self.runtimes[target.replace("-linux-gnu", "-linux-musl")],
            linux_runtime=self.runtimes[package.LINUX],
            linux_arm64_runtime=self.runtimes[package.LINUX_ARM64],
            windows_runtime=self.runtimes[package.WINDOWS],
            windows_arm64_runtime=self.runtimes[package.WINDOWS_ARM64],
            macos_runtime=self.runtimes[package.MACOS], without_agent_deck=False,
        )

    def archive(self, product, target):
        extension = "zip" if target in package.WINDOWS_TARGETS else "tar.gz"
        return self.output / f"corvid-{product}-{self.version}-{target}.{extension}"

    def test_packages_native_runtime_and_all_remote_architectures(self):
        for target in (package.DESKTOP_LINUX, package.DESKTOP_LINUX_ARM64, *package.WINDOWS_TARGETS):
            with self.subTest(target=target):
                package.package(self.arguments("desktop", target))
                path = self.archive("desktop", target)
                if target in package.WINDOWS_TARGETS:
                    with zipfile.ZipFile(path) as archive:
                        contents = {name: archive.read(name) for name in archive.namelist()}
                    binary = "corvid.exe"
                else:
                    with tarfile.open(path) as archive:
                        contents = {entry.name: archive.extractfile(entry).read() for entry in archive}
                    binary = "corvid"
                self.assertEqual(contents[binary], target.replace("-linux-gnu", "-linux-musl").encode())
                daemon = binary.replace("corvid", "corvidd")
                self.assertEqual(contents[daemon], f"daemon:{target.replace('-linux-gnu', '-linux-musl')}".encode())
                self.assertEqual(json.loads(contents["package.json"])["target"], target)
                self.assertEqual(contents["STIX-OFL.txt"],
                                 (package.ROOT / "assets/fonts/STIX-OFL.txt").read_bytes())
                for remote in package.RUNTIME_TARGETS:
                    manifest = json.loads(contents[f"daemon/{remote}/manifest.json"])
                    self.assertEqual(manifest["target"], remote)
                    remote_name = "corvid.exe" if remote in package.WINDOWS_TARGETS else "corvid"
                    self.assertEqual(contents[f"daemon/{remote}/{remote_name}"], f"daemon:{remote}".encode())

    def test_rejects_swapped_or_missing_arm64_runtimes(self):
        args = self.arguments("desktop", package.WINDOWS_ARM64)
        args.runtime = self.runtimes[package.WINDOWS]
        with self.assertRaisesRegex(ValueError, "Runtime identity mismatch"):
            package.package(args)
        args = self.arguments("desktop", package.DESKTOP_LINUX)
        args.linux_arm64_runtime = None
        with self.assertRaisesRegex(ValueError, "Desktop requires the aarch64"):
            package.package(args)
        args.linux_arm64_runtime = self.runtimes[package.LINUX]
        with self.assertRaisesRegex(ValueError, "Runtime identity mismatch"):
            package.package(args)

    def test_setup_provenance_preserves_architecture(self):
        def compile_setup(payload, version, output, compiler, target):
            (output / native_package.asset_name(version, target)).write_bytes(target.encode())

        with patch("native_package.windows_package", side_effect=compile_setup):
            for target in package.WINDOWS_TARGETS:
                package.package(self.arguments("desktop", target))
                native_package.build(self.archive("desktop", target), self.output, None)
                name = native_package.verify(self.output, self.version, self.revision, target)
                self.assertIn(target, name)
        path = self.output / (native_package.asset_name(self.version, package.WINDOWS_ARM64) + ".provenance.json")
        provenance = json.loads(path.read_text())
        provenance["target"] = package.WINDOWS
        package.write_json(path, provenance)
        with self.assertRaisesRegex(ValueError, "provenance mismatch"):
            native_package.verify(self.output, self.version, self.revision, package.WINDOWS_ARM64)

    def test_release_manifest_requires_arm64_assets(self):
        # This test checks the inventory, not native app signing or execution.
        with patch("macos_package.app_bundle"), patch("native_package.verify", return_value="setup.exe"), patch("macos_package.verify", return_value="Corvid.dmg"):
            for product, targets in [("tui", package.RUNTIME_TARGETS), ("desktop", package.DESKTOP_TARGETS)]:
                for target in targets:
                    package.package(self.arguments(product, target))
            with patch("package.validate_runtime_binary"):
                for target in package.RUNTIME_TARGETS:
                    filename = "corvid.exe" if target in package.WINDOWS_TARGETS else "corvid"
                    package.daemon_asset(SimpleNamespace(target=target, output=self.output,
                                                         binary=self.runtimes[target] / filename))
            package.manifest(SimpleNamespace(output=self.output))
            manifest = json.loads((self.output / "release-manifest.json").read_text())
            self.assertEqual(len(manifest["assets"]), 17)  # Archives, scripts and standalone runtimes.
            for target in package.RUNTIME_TARGETS:
                name = package.daemon_asset_name(target, self.version)
                asset = next(asset for asset in manifest["assets"] if asset["name"] == name)
                self.assertEqual(asset["sha256"], package.digest(self.output / name))
            daemon = self.output / package.daemon_asset_name(package.LINUX, self.version)
            original = daemon.read_bytes()
            daemon.write_bytes(b"invalid daemon")
            with self.assertRaisesRegex(ValueError, "Runtime integrity mismatch"):
                package.manifest(SimpleNamespace(output=self.output))
            daemon.write_bytes(original)
            self.archive("tui", package.LINUX_ARM64).unlink()
            with self.assertRaises(FileNotFoundError):
                package.manifest(SimpleNamespace(output=self.output))


if __name__ == "__main__":
    unittest.main()
