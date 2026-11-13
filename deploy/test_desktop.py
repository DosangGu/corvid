#!/usr/bin/env python3
"""Launch the packaged Linux Desktop under an isolated X11 display."""
import os
from pathlib import Path
import re
import shlex
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
    environment = dict(os.environ, CORVID_RUNTIME_DIR=str(runtime / "corvid"), XDG_STATE_HOME=str(root / "state"), SHELL="/bin/sh")
    # A foreground daemon keeps this test in control of its process lifecycle.
    daemon = subprocess.Popen([str(root / "corvid"), "daemon"], env=environment, start_new_session=True)
    manager = subprocess.Popen(["openbox"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    desktop = None
    previous = None
    try:
        for _ in range(80):
            ping = subprocess.run([str(root / "corvid"), "control", "ping"], env=environment, capture_output=True)
            if ping.returncode == 0:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("Daemon did not start")

        # Openbox starts asynchronously. xdotool needs its EWMH activation
        # support, even if a Desktop window is already visible on the display.
        for _ in range(80):
            if manager.poll() is not None:
                raise AssertionError(f"Openbox exited with {manager.returncode}")
            supported = subprocess.check_output(["xprop", "-root", "_NET_SUPPORTED"], text=True)
            if "_NET_ACTIVE_WINDOW" in supported:
                break
            time.sleep(0.1)
        else:
            raise AssertionError("Openbox did not register window activation support")

        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        def wait_for_icon(title, excluded=()):
            for _ in range(80):
                if desktop.poll() is not None:
                    raise AssertionError(f"Desktop exited with {desktop.returncode}")
                windows = subprocess.check_output(["xwininfo", "-root", "-tree"], text=True)
                matches = re.findall(r'(0x[0-9a-f]+) "' + re.escape(title) + '"', windows)
                for window in matches:
                    if window in excluded:
                        continue
                    properties = subprocess.check_output(
                        ["xprop", "-id", window, "WM_CLASS", "_NET_WM_ICON"], text=True,
                    )
                    if ('"Corvid.Desktop"' in properties and "_NET_WM_ICON(CARDINAL)" in properties
                            and "32 x 32" in properties and "128 x 128" in properties):
                        return window
                time.sleep(0.25)
            raise AssertionError(f"{title} did not open with its app identity and icon")

        window = wait_for_icon("Corvid Desktop")
        def wait_for_writer(retired=None):
            for _ in range(80):
                clients = subprocess.check_output(
                    [str(root / "corvid"), "control", "clients"], env=environment, text=True,
                ).splitlines()
                writers = [line.split()[0] for line in clients if "pane-writer=" in line]
                if len(writers) == 1 and writers[0] != retired:
                    return writers[0]
                time.sleep(0.1)
            raise AssertionError("Desktop did not acquire the pane's input route")

        def write_marker(window, marker, command):
            subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
            # Wait for the initial full terminal frame after the writer is published.
            time.sleep(0.5)
            subprocess.run(["xdotool", "type", "--clearmodifiers", "--", command], check=True, timeout=10)
            subprocess.run(["xdotool", "key", "Return"], check=True, timeout=10)
            for _ in range(80):
                if marker.exists():
                    assert marker.read_text() == "preserved", "Desktop replaced the existing shell"
                    return
                time.sleep(0.1)
            raise AssertionError("Desktop did not forward input to the terminal")

        retired = wait_for_writer()
        first_marker = root / "first"
        write_marker(window, first_marker, f"kept=preserved; printf %s \"$kept\" > {shlex.quote(str(first_marker))}")
        previous = desktop
        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        window = wait_for_icon("Corvid Desktop", excluded=(window,))
        current = wait_for_writer(retired)
        resumed_marker = root / "resumed"
        write_marker(window, resumed_marker, f"printf %s \"$kept\" > {shlex.quote(str(resumed_marker))}")
        assert wait_for_writer() == current, "The previous Desktop reclaimed input automatically"
        subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
        subprocess.run(["xdotool", "key", "ctrl+comma"], check=True, timeout=10)
        wait_for_icon("Corvid Settings")
        print("PASS Desktop resumed the existing shell directly; Desktop and Settings have X11 icons and app identity")
    finally:
        for process in [desktop, previous, daemon, manager]:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=15)
