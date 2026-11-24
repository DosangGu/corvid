#!/usr/bin/env python3
"""Run the Desktop lifecycle test inside a disposable, real WSL 2 distribution."""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import uuid


ROOT = Path(__file__).resolve().parent.parent
TEST = "terminal_session::wsl_test::wsl_session_installs_and_reconnects_to_the_same_shell"


def run_wsl_phase(name, phase, command, *, timeout):
    print(f"WSL {name}: {phase} ({timeout}s timeout)", flush=True)
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL)
    try:
        status = process.wait(timeout=timeout)
    except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        # subprocess.run() waits without a deadline after killing a Windows child.
        try:
            process.kill()
            process.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired) as termination_error:
            error.add_note(f"Could not stop WSL {phase} process: {termination_error}")
        raise
    if status:
        raise subprocess.CalledProcessError(status, command)


@contextmanager
def imported_distro(wsl, directory, rootfs):
    name = f"corvid-wsl-test-{uuid.uuid4().hex[:10]}"
    try:
        run_wsl_phase(name, "importing distribution",
                      [wsl, "--import", name, directory / "linux", rootfs, "--version", "2"],
                      timeout=120)
        yield name, [wsl, "--distribution", name, "--user", "root", "--exec"]
    finally:
        # A failed import can leave a registered distribution under this name.
        run_wsl_phase(name, "unregistering distribution", [wsl, "--unregister", name], timeout=60)


def test_session(directory, name, linux, desktop, daemon):
    architecture = subprocess.check_output([*linux, "uname", "-m"], timeout=20).decode().strip()
    if architecture not in ("x86_64", "aarch64"):
        raise RuntimeError(f"unsupported WSL test architecture {architecture!r}")
    target = f"{architecture}-unknown-linux-musl"
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    runtime = f"/root/{name}"
    subprocess.run([*linux, "mkdir", "-m", "700", runtime], timeout=20, check=True)

    executable = directory / "desktop-test.exe"
    shutil.copy2(desktop, executable)
    bundle = directory / "daemon" / target
    bundle.mkdir(parents=True)
    payload = bundle / "corvid"
    shutil.copy2(daemon, payload)
    with payload.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    (bundle / "manifest.json").write_text(json.dumps({
        "schema_version": 1, "target": target, "version": version,
        "size": payload.stat().st_size, "sha256": digest,
    }), encoding="utf-8")

    forwarded = {"CORVID_RUNTIME_DIR": f"{runtime}/run", "XDG_STATE_HOME": f"{runtime}/state"}
    wslenv = [entry for entry in os.environ.get("WSLENV", "").split(":")
              if entry and entry.split("/", 1)[0] not in forwarded]
    env = os.environ | forwarded | {"WSLENV": ":".join([*wslenv, *forwarded]),
                                     "CORVID_WSL_TEST_DISTRIBUTION": name}
    result = subprocess.run([executable, "--ignored", "--exact", TEST, "--nocapture"],
                            timeout=180, env=env, stdout=subprocess.PIPE, text=True)
    print(result.stdout, end="", flush=True)
    result.check_returncode()
    if "1 passed" not in result.stdout:
        raise RuntimeError("the WSL lifecycle test did not run")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rootfs", type=Path, required=True, help="trusted Linux rootfs archive")
    parser.add_argument("--desktop-test", type=Path, help="Windows corvid-desktop test executable")
    parser.add_argument("--daemon", type=Path, help="matching Linux musl corvidd binary")
    parser.add_argument("--probe", action="store_true", help="only import and boot the test distribution")
    args = parser.parse_args()
    if not args.probe and (args.desktop_test is None or args.daemon is None):
        parser.error("supply --desktop-test and --daemon for the full lifecycle")
    if os.name != "nt":
        parser.error("the lifecycle requires a Windows host with WSL 2")
    wsl = Path(os.environ["SystemRoot"]) / "System32" / "wsl.exe"
    with tempfile.TemporaryDirectory(prefix="corvid-wsl-") as temporary:
        directory = Path(temporary)
        with imported_distro(wsl, directory, args.rootfs.resolve(strict=True)) as (name, linux):
            if args.probe:
                run_wsl_phase(name, "booting distribution", [*linux, "uname", "-sr"], timeout=30)
            else:
                test_session(directory, name, linux, args.desktop_test, args.daemon)


if __name__ == "__main__":
    main()
