#!/usr/bin/env python3
"""Exercise Linux Desktop UI interactions under an isolated X11 display."""
import argparse
import os
import platform
import json
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import tarfile
import tempfile
import time
import monitor_fixture

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("assets", type=Path, nargs="?", help="directory containing the Desktop archive")
parser.add_argument("--build", type=Path, help="directory containing built Desktop and runtime binaries")
args = parser.parse_args()
if (args.assets is None) == (args.build is None):
    parser.error("provide either an assets directory or --build")

with (tempfile.TemporaryDirectory(prefix=".corvid-desktop-smoke-", dir=Path.home()) as directory,
      tempfile.TemporaryDirectory(prefix=".cvr-", dir=Path.home()) as runtime_directory):
    root = Path(directory)
    if args.build:
        for binary in ("corvid", "corvid-desktop", "corvid-agent-deck"):
            shutil.copy2(args.build / binary, root / binary)
    else:
        archive, = args.assets.glob(f"corvid-desktop-*-{platform.machine()}-unknown-linux-gnu.tar.gz")
        with tarfile.open(archive) as stream:
            stream.extractall(root, filter="data")
    runtime = Path(runtime_directory)
    environment = dict(os.environ, HOME=str(root), CORVID_RUNTIME_DIR=str(runtime / "corvid"),
                       CORVID_VIEW_STATE_DIR=str(root / "views"), XDG_STATE_HOME=str(root / "state"),
                       SHELL="/bin/sh", XDG_RUNTIME_DIR=str(runtime))
    monitor_state = root / "state/corvid/companion"
    # A foreground daemon keeps this test in control of its process lifecycle.
    daemon = subprocess.Popen([str(root / "corvid"), "daemon"], env=environment, start_new_session=True)
    manager = subprocess.Popen(["openbox"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    desktop = None
    previous = None
    deck = None
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
        reader, monitor_epoch = monitor_fixture.ready(root / "corvid", monitor_state, environment)
        def writers():
            clients = subprocess.check_output(
                [str(root / "corvid"), "control", "clients"], env=environment, text=True, timeout=5,
            ).splitlines()
            return [line for line in clients if "pane-writer=" in line]

        def wait_for_writer(retired=None):
            for _ in range(80):
                active = writers()
                if len(active) == 1 and active[0].split()[0] != retired:
                    return active[0].split()[0]
                time.sleep(0.1)
            raise AssertionError("Desktop did not acquire the pane's input route")

        def wait_for_terminal(*, previous=None, expected=None):
            for _ in range(80):
                active = writers()
                if len(active) == 1 and (match := re.search(r" terminal=(\S+)", active[0])):
                    terminal = match[1]
                    if terminal != previous and (expected is None or terminal == expected):
                        return terminal
                time.sleep(0.1)
            raise AssertionError("Desktop did not select the expected tab's terminal")

        def wait_for_marker(marker, expected):
            for _ in range(80):
                if marker.exists() and marker.read_text(encoding="utf-8") == expected:
                    return
                assert desktop.poll() is None, "Desktop exited while forwarding input"
                time.sleep(0.1)
            raise AssertionError(f"Desktop did not produce {expected!r} in {marker.name}")

        def write_marker(window, marker, command):
            subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
            # Wait for the initial full terminal frame after the writer is published.
            time.sleep(0.5)
            subprocess.run(["xdotool", "type", "--clearmodifiers", "--", command], check=True, timeout=10)
            subprocess.run(["xdotool", "key", "Return"], check=True, timeout=10)
            wait_for_marker(marker, "preserved")

        def key(window, chord):
            subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
            subprocess.run(["xdotool", "key", "--clearmodifiers", chord], check=True, timeout=10)

        retired = wait_for_writer()
        first_marker = root / "first"
        write_marker(window, first_marker, f"kept=preserved; printf %s \"$kept\" > {shlex.quote(str(first_marker))}")
        # Use the real clipboard and keyboard path, with synthetic Unicode data.
        pasted = "한글 e\u0301 🐦"
        paste_marker = root / "pasted"
        paste_command = f"printf %s {shlex.quote(pasted)} > {shlex.quote(str(paste_marker))}"
        subprocess.run(["xclip", "-selection", "clipboard"], input=paste_command.encode("utf-8"),
                       check=True, timeout=10)
        subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+shift+v"], check=True, timeout=10)
        subprocess.run(["xdotool", "key", "Return"], check=True, timeout=10)
        wait_for_marker(paste_marker, pasted)
        first_terminal = wait_for_terminal()
        key(window, "ctrl+shift+t")
        wait_for_terminal(previous=first_terminal)
        second_tab_marker = root / "second-tab"
        write_marker(window, second_tab_marker,
                     f"second=preserved; printf %s \"$second\" > {shlex.quote(str(second_tab_marker))}")
        key(window, "ctrl+shift+Tab")
        wait_for_terminal(expected=first_terminal)
        selected_marker = root / "selected-tab"
        write_marker(window, selected_marker, f"printf %s \"$kept\" > {shlex.quote(str(selected_marker))}")
        first_window = window
        previous = desktop
        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        window = wait_for_icon("Corvid Desktop", excluded=(window,))
        time.sleep(0.5)
        assert wait_for_writer() == retired, "A new window displaced the existing writer"

        # The overflow menu is in the top-right of the 1080px client area.
        key(window, "Escape")  # Dismiss the picker before opening the menu.
        time.sleep(0.1)
        subprocess.run(["xdotool", "mousemove", "--window", window, "1055", "20", "click", "1"], check=True, timeout=10)
        time.sleep(0.3)
        if screenshot := os.environ.get("CORVID_DESKTOP_SCREENSHOT"):
            # Capture the empty window's menu, never a user's terminal content.
            subprocess.run(["scrot", "--overwrite", "--focused", screenshot], check=True, timeout=10)
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
        assert desktop.wait(timeout=15) == 0, "Desktop failed while quitting"
        assert previous.poll() is None, "Quitting one Desktop process closed another"
        assert wait_for_writer() == retired
        key(first_window, "ctrl+shift+q")
        assert previous.wait(timeout=15) == 0, "The original Desktop failed while quitting"
        monitor_fixture.assert_same(reader, monitor_epoch, environment)

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
        assert desktop.wait(timeout=15) == 0, "Desktop failed while closing its last window"
        wait_for_saved_windows(1)
        desktop = subprocess.Popen([str(root / "corvid-desktop")], env=environment, start_new_session=True)
        window = wait_for_icon("Corvid Desktop")
        wait_for_writer(current)
        final_marker = root / "final"
        write_marker(window, final_marker, f"printf %s \"$kept\" > {shlex.quote(str(final_marker))}")
        # Open Deck after the monitor already synchronized. Only this isolated,
        # empty deck is captured; terminal windows are excluded from the image.
        deck_binary = root / ("corvid-agent-deck" if args.build else "plugins/agent-deck/corvid-agent-deck")
        deck = subprocess.Popen([str(deck_binary), "--state-dir", str(monitor_state)],
                                env=environment, start_new_session=True)
        for _ in range(80):
            assert deck.poll() is None, f"Agent Deck exited with {deck.returncode}"
            windows = subprocess.check_output(["xwininfo", "-root", "-tree"], text=True)
            if match := re.search(r'(0x[0-9a-f]+) "Corvid Agent Deck"', windows):
                deck_window = match[1]
                break
            time.sleep(0.1)
        else:
            raise AssertionError("Agent Deck did not open")
        monitor_fixture.assert_same(reader, monitor_epoch, environment)
        if screenshot := os.environ.get("CORVID_AGENT_DECK_SCREENSHOT"):
            subprocess.run(["xdotool", "windowactivate", "--sync", deck_window], check=True, timeout=10)
            time.sleep(1)
            subprocess.run(["scrot", "--overwrite", "--focused", screenshot], check=True, timeout=10)
        print("PASS Desktop keyboard input, Unicode clipboard paste, tab creation/switching, independent windows, new-window menu, close/quit/relaunch, shell continuity, settings, and X11 icons")
    finally:
        for process in [deck, desktop, previous, daemon, manager]:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=15)
        monitor_fixture.stop(root / "corvid", monitor_state, environment)
