#!/usr/bin/env python3
"""Check successful managed SSH connections against an older installation.

Uses real daemon/relay handshakes and a local SSH command shim. Upgrade dispatch
is verified without replacing the fixture daemon; this is not a historical
binary compatibility or network transport test.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import signal
import subprocess
import tempfile
import time


def stop(process):
    if process is None or process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def verify_default_and_capture(root, environment, output):
    manager = desktop = None
    try:
        manager = subprocess.Popen(["openbox"], start_new_session=True)
        desktop = subprocess.Popen(
            [str(root / "corvid-desktop"), "--ssh-alias", "corvid-update-test"],
            env=environment, start_new_session=True,
        )
        for _ in range(160):
            if desktop.poll() is not None:
                raise AssertionError("Desktop exited before the update prompt")
            windows = subprocess.check_output(["xwininfo", "-root", "-tree"], text=True)
            match = re.search(r'(0x[0-9a-f]+) "Corvid Desktop"', windows)
            if match and (root / "version-queried").exists():
                # The prompt is drawn on the next UI frames after the version probe.
                time.sleep(2)
                subprocess.run(["xdotool", "windowactivate", "--sync", match[1]], check=True)
                subprocess.run(["scrot", "--overwrite", "--focused", str(output)], check=True)
                upgraded = root / "upgraded.json"
                assert not upgraded.exists(), "The upgrade started before confirmation"
                subprocess.run(["xdotool", "key", "Return"], check=True, timeout=5)
                for _ in range(160):
                    if upgraded.exists() and (root / "installed-version").read_text() != "0.1.2":
                        assert json.loads(upgraded.read_text())["verified_payload"]
                        upgraded.unlink()
                        return
                    time.sleep(0.1)
                raise AssertionError("Enter did not approve the default Upgrade action")
            time.sleep(0.1)
        raise AssertionError("Desktop did not check the connected remote version")
    finally:
        stop(desktop)
        stop(manager)


def verify(args, root, test_binary):
    build = args.build.resolve(strict=True)
    for name in ("corvid", "corvidd", "corvid-desktop", "corvid-agent-deck"):
        shutil.copy2(build / name, root / name)
    shutil.copy2(test_binary, root / "test-runner")
    current = subprocess.check_output([str(root / "corvid"), "--version"], text=True).split()[1]
    target = f"{platform.machine()}-unknown-linux-musl"
    bundle = root / "daemon" / target
    bundle.mkdir(parents=True)
    payload = (root / "corvidd").read_bytes()
    # The fixture only verifies dispatch, so the host-native test binary can stand
    # in for the release's musl bundle without claiming musl runtime coverage.
    (bundle / "corvid").write_bytes(payload)
    (bundle / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "target": target, "version": current,
        "size": len(payload), "sha256": hashlib.sha256(payload).hexdigest(),
    }))
    (root / "bundle-file").write_text(str(bundle / "corvid"))
    (root / ".corvid").mkdir(mode=0o700)
    (root / "bin").mkdir()
    (root / "path-bin").mkdir()
    (root / "installed-version").write_text("0.1.2")
    installed = root / ".corvid/corvid"
    installed.write_text(
        '#!/bin/sh\nif [ "$1" = --version ]; then\n'
        f'  touch {shlex.quote(str(root / "version-queried"))}\n'
        f'  printf "corvid %s\\n" "$(cat {shlex.quote(str(root / "installed-version"))})"\n'
        'else\n'
        f'  exec {shlex.quote(str(root / "corvid"))} "$@"\n'
        'fi\n'
    )
    installed.chmod(0o700)
    (root / "path-bin/corvid").symlink_to(installed)
    shim = root / "bin/ssh"
    shim.write_text(
        '#!/usr/bin/python3\n'
        'import hashlib, json, os, pathlib, shlex, sys\n'
        f'root = pathlib.Path({str(root)!r})\n'
        'command = sys.argv[-1]\n'
        'args = shlex.split(command)\n'
        'if len(args) > 4 and args[3:5] == ["corvid-ssh", "upgrade"]:\n'
        '    size, digest = sys.stdin.buffer.readline().split()\n'
        '    payload = sys.stdin.buffer.read(int(size))\n'
        '    verified = len(payload) == int(size) and hashlib.sha256(payload).hexdigest() == digest.decode()\n'
        '    assert verified\n'
        '    (root / "upgraded.json").write_text(json.dumps({"previous_version": args[5], '
        '"expected_host": args[6], "verified_payload": verified}))\n'
        f'    (root / "installed-version").write_text({current!r})\n'
        '    sys.exit(0)\n'
        'os.environ["PATH"] = "/usr/bin:/bin"\n'
        'if (root / "path-installation").exists():\n'
        '    os.environ["PATH"] = str(root / "path-bin") + ":" + os.environ["PATH"]\n'
        'os.execv("/bin/sh", ["sh", "-c", command])\n'
    )
    shim.chmod(0o700)
    environment = os.environ | {
        "HOME": str(root), "CORVID_RUNTIME_DIR": str(root / "r"),
        "CORVID_VIEW_STATE_DIR": str(root / "views"), "XDG_STATE_HOME": str(root / "state"),
        "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
        "CORVID_REMOTE_UPDATE_FIXTURE": str(root),
    }
    daemon = None
    try:
        daemon = subprocess.Popen(
            [str(root / "corvid"), "daemon", "--autostarted"],
            env=environment, start_new_session=True,
        )
        for _ in range(100):
            if daemon.poll() is not None:
                raise AssertionError("Fixture daemon exited")
            result = subprocess.run(
                [str(root / "corvid"), "control", "ping"], env=environment,
                capture_output=True, timeout=5,
            )
            if result.returncode == 0:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("Fixture daemon did not start")
        if args.screenshot:
            verify_default_and_capture(root, environment, args.screenshot.resolve())
        subprocess.run(
            [str(root / "test-runner"), "--ignored", "--exact", "--nocapture",
             "remote_bootstrap::tests::connected_managed_installations_offer_updates"],
            env=environment, check=True, timeout=120,
        )
    finally:
        stop(daemon)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--screenshot", type=Path)
    args = parser.parse_args()
    result = subprocess.run(
        ["cargo", "test", "--locked", "--offline", "--release", "--target", args.target,
         "-p", "corvid-desktop", "--bin", "corvid-desktop", "--no-run", "--message-format=json"],
        capture_output=True, text=True, check=True,
    )
    artifacts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
    test_binary, = [Path(item["executable"]) for item in artifacts
                    if item.get("reason") == "compiler-artifact" and item.get("executable")
                    and item.get("profile", {}).get("test")]
    with tempfile.TemporaryDirectory(prefix="cv-up-", dir=Path.home()) as temporary:
        verify(args, Path(temporary), test_binary)


if __name__ == "__main__":
    main()
