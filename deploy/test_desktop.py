#!/usr/bin/env python3
"""Launch the packaged Linux Desktop under an isolated X11 display."""
import os
import json
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
            for _ in range(240):
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
            raise AssertionError(f"{title} did not open with its app identity and icon:\n{windows}")

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
        first_window = window
        previous = desktop
        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        window = wait_for_icon("Corvid Desktop", excluded=(window,))
        time.sleep(0.5)
        assert wait_for_writer() == retired, "A new window displaced the existing writer"

        def key(window, chord):
            subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
            subprocess.run(["xdotool", "key", "--clearmodifiers", chord], check=True, timeout=10)

        # The overflow menu is in the top-right of the 1080px client area.
        subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
        subprocess.run(["xdotool", "mousemove", "--window", window, "1055", "20", "click", "1"], check=True, timeout=10)
        time.sleep(0.3)
        if screenshot := os.environ.get("CORVID_DESKTOP_SCREENSHOT"):
            # Capture the empty window's menu, never a user's terminal content.
            subprocess.run(["scrot", "--focused", screenshot], check=True, timeout=10)
        key(window, "Return")  # New window is the first menu entry.
        extra = wait_for_icon("Corvid Desktop", excluded=(first_window, window))
        manifest = root / "state/corvid/companion/desktop-view/windows.json"

        def wait_for_saved_windows(count):
            for _ in range(80):
                if len(json.loads(manifest.read_text())["windows"]) == count:
                    return
                time.sleep(0.1)
            raise AssertionError(f"Desktop did not save {count} windows")

        wait_for_saved_windows(3)
        key(extra, "alt+F4")
        wait_for_saved_windows(2)
        key(window, "ctrl+shift+q")
        desktop.wait(timeout=15)
        assert previous.poll() is None, "Quitting one Desktop process closed another"
        assert wait_for_writer() == retired
        key(first_window, "ctrl+shift+q")
        previous.wait(timeout=15)

        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        restored = wait_for_icon("Corvid Desktop")
        other = wait_for_icon("Corvid Desktop", excluded=(restored,))
        wait_for_saved_windows(2)
        current = wait_for_writer(retired)
        # Windows are allocated in manifest order on the same X connection.
        window, picker = sorted((restored, other), key=lambda value: int(value, 16))
        resumed_marker = root / "resumed"
        write_marker(window, resumed_marker, f"printf %s \"$kept\" > {shlex.quote(str(resumed_marker))}")
        assert wait_for_writer() == current
        key(window, "ctrl+comma")
        wait_for_icon("Corvid Settings")
        key(picker, "alt+F4")
        wait_for_saved_windows(1)
        key(window, "alt+F4")  # Closing the final terminal window preserves it.
        desktop.wait(timeout=15)
        wait_for_saved_windows(1)
        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        window = wait_for_icon("Corvid Desktop")
        wait_for_writer(current)
        final_marker = root / "final"
        write_marker(window, final_marker, f"printf %s \"$kept\" > {shlex.quote(str(final_marker))}")
        print("PASS independent Desktop windows, new-window menu, close/quit/relaunch, shell continuity, and X11 icons")
    finally:
        for process in [desktop, previous, daemon, manager]:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=15)
