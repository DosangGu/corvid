#!/usr/bin/env python3
"""Launch the packaged Linux Desktop under an isolated X11 display."""
import os
from pathlib import Path
import re
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
        def wait_for_icon(title):
            for _ in range(80):
                if desktop.poll() is not None:
                    raise AssertionError(f"Desktop exited with {desktop.returncode}")
                windows = subprocess.check_output(["xwininfo", "-root", "-tree"], text=True)
                match = re.search(r'(0x[0-9a-f]+) "' + re.escape(title) + '"', windows)
                if match:
                    window = match.group(1)
                    properties = subprocess.check_output(
                        ["xprop", "-id", window, "WM_CLASS", "_NET_WM_ICON"], text=True,
                    )
                    if ('"Corvid.Desktop"' in properties and "_NET_WM_ICON(CARDINAL)" in properties
                            and "32 x 32" in properties and "128 x 128" in properties):
                        return window
                time.sleep(0.25)
            raise AssertionError(f"{title} did not open with its app identity and icon")

        window = wait_for_icon("Corvid Desktop")
        subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
        subprocess.run(["xdotool", "key", "ctrl+comma"], check=True, timeout=10)
        wait_for_icon("Corvid Settings")
        print("PASS packaged Linux Desktop and Settings opened with X11 icons and app identity")
    finally:
        for process in [desktop, daemon, manager]:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=15)
