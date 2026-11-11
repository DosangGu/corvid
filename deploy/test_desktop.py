#!/usr/bin/env python3
"""Launch the packaged Linux Desktop under an isolated X11 display."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import tarfile
import tempfile
import time

with tempfile.TemporaryDirectory(prefix=".corvid-desktop-smoke-", dir=Path.home()) as directory:
    root = Path(directory)
    archive = next(Path(sys.argv[1]).glob("corvid-desktop-*-x86_64-unknown-linux-gnu.tar.gz"))
    with tarfile.open(archive) as stream:
        stream.extractall(root, filter="data")
    runtime = root / "runtime"
    runtime.mkdir(mode=0o700)
    environment = dict(os.environ, CORVID_RUNTIME_DIR=str(runtime / "corvid"), XDG_STATE_HOME=str(root / "state"))
    # A foreground daemon keeps this test in control of its process lifecycle.
    daemon = subprocess.Popen([str(root / "corvid"), "daemon"], env=environment, start_new_session=True)
    manager = subprocess.Popen(["openbox"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    desktop = None
    try:
        for _ in range(80):
            ping = subprocess.run([str(root / "corvid"), "control", "ping"], env=environment, capture_output=True)
            if ping.returncode == 0:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("Daemon did not start")
        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        for _ in range(80):
            if desktop.poll() is not None:
                raise AssertionError(f"Desktop exited with {desktop.returncode}")
            windows = subprocess.check_output(["xwininfo", "-root", "-tree"], text=True)
            if '"Corvid Desktop"' in windows:
                print("PASS packaged Linux Desktop opened an X11 window")
                break
            time.sleep(0.25)
        else:
            raise AssertionError("Desktop did not open a window")
    finally:
        for process in [desktop, daemon, manager]:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=15)
