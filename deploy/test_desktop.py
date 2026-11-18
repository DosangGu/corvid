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
    shell_started = root / "shell-started"
    keyboard_ready = root / "keyboard-ready"
    shell_init = root / ".shell-init"
    shell_init.write_text(
        f": > {shlex.quote(str(shell_started))}\n"
        f"corvid_fixture_ready() {{ printf %s \"$1\" > {shlex.quote(str(keyboard_ready))}; }}\n",
        encoding="utf-8",
    )
    environment = dict(os.environ, HOME=str(root), CORVID_RUNTIME_DIR=str(runtime / "corvid"),
                       CORVID_VIEW_STATE_DIR=str(root / "views"), XDG_STATE_HOME=str(root / "state"),
                       SHELL="/bin/sh", ENV=str(shell_init), XDG_RUNTIME_DIR=str(runtime))
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
                        wait_for_keyboard(window, active[0].split()[0])
                        if writers() == active:
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

        def type_command(command):
            subprocess.run(["xdotool", "type", "--clearmodifiers", "--", command], check=True, timeout=10)
            subprocess.run(["xdotool", "key", "Return"], check=True, timeout=10)

        def x11_window_matches(query, window):
            result = subprocess.run(["xdotool", query], capture_output=True, text=True, timeout=5)
            output = result.stdout.strip()
            return result.returncode == 0 and output.isdecimal() and int(output) == int(window, 16)

        def input_status(window, expected_writer, readiness_token):
            try:
                active = writers()
                writer_matches = len(active) == 1 and active[0].split()[0] == expected_writer
            except subprocess.SubprocessError:
                writer_matches = "unavailable"
            return (
                f"shell started={shell_started.exists()}, "
                f"X11 active={x11_window_matches('getactivewindow', window)}, "
                f"X11 keyboard focus={x11_window_matches('getwindowfocus', window)}, "
                f"expected writer={writer_matches}, "
                f"readiness command received={probe_received(readiness_token)}"
            )

        def probe_received(readiness_token):
            return (
                keyboard_ready.exists()
                and keyboard_ready.read_text(encoding="utf-8") == readiness_token
            )

        def wait_for_keyboard(window, expected_writer):
            subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
            # Each transition must acknowledge its own readiness command.
            readiness_token = str(time.monotonic_ns())
            deadline = time.monotonic() + 15
            next_probe = 0.0
            while time.monotonic() < deadline and desktop.poll() is None:
                if probe_received(readiness_token):
                    return readiness_token
                if (time.monotonic() >= next_probe and shell_started.exists()
                        and x11_window_matches("getactivewindow", window)
                        and x11_window_matches("getwindowfocus", window)):
                    # A published writer and X11 focus can precede usable keyboard input.
                    # Clear any incomplete probe before sending another idempotent command.
                    subprocess.run(
                        ["xdotool", "key", "--clearmodifiers", "ctrl+u"], check=True, timeout=10,
                    )
                    type_command(f"corvid_fixture_ready {readiness_token}")
                    next_probe = time.monotonic() + 0.5
                time.sleep(0.05)
            raise AssertionError(
                f"Desktop keyboard input did not become ready ({input_status(window, expected_writer, readiness_token)})"
            )

        def write_marker(window, marker, command):
            wait_for_keyboard(window, wait_for_writer())
            type_command(command)
            wait_for_marker(marker, "preserved")

        def key(window, chord):
            subprocess.run(["xdotool", "windowactivate", "--sync", window], check=True, timeout=10)
            subprocess.run(["xdotool", "key", "--clearmodifiers", chord], check=True, timeout=10)

        retired = wait_for_writer()
        first_probe = wait_for_keyboard(window, retired)
        first_marker = root / "first"
        type_command(f"kept=preserved; printf %s \"$kept\" > {shlex.quote(str(first_marker))}")
        try:
            wait_for_marker(first_marker, "preserved")
        except AssertionError as error:
            raise AssertionError(
                f"{error} ({input_status(window, retired, first_probe)})"
            ) from error
        # Scrollback must release the keyboard at the bottom, and an ordinary
        # foreground program that never enabled mouse reporting must keep it.
        def wheel(button, count):
            subprocess.run(["xdotool", "mousemove", "--window", window, "320", "240",
                            "click", "--repeat", str(count), "--delay", "15", str(button)],
                           check=True, timeout=10)

        history_ready = root / "history-ready"
        type_command(f"seq -f 'scroll-row-%03g' 1 240; printf ready > {shlex.quote(str(history_ready))}")
        wait_for_marker(history_ready, "ready")
        wheel(4, 30)
        key(window, "Home")
        key(window, "ctrl+shift+c")
        copied = subprocess.check_output(["xclip", "-selection", "clipboard", "-o"], timeout=10)
        assert b"scroll-row-001" in copied.splitlines(), "wheel did not open shell history"
        if preview := os.environ.get("CORVID_SCROLL_PREVIEW_DIR"):
            subprocess.run(["scrot", "--overwrite", "--focused", str(Path(preview) / "scroll-history.png")],
                           check=True, timeout=10)
        wheel(5, 260)
        live = root / "scroll-live"
        type_command(f"printf live > {shlex.quote(str(live))}")
        wait_for_marker(live, "live")
        if preview:
            subprocess.run(["scrot", "--overwrite", "--focused", str(Path(preview) / "scroll-live.png")],
                           check=True, timeout=10)

        program_ready = root / "program-ready"
        program_input = root / "program-input"
        program = root / "reader.py"
        program.write_text(
            "from pathlib import Path\n"
            "print('\\033[2J\\033[H\\033[3J', end='')\n"
            "for row in range(1, 301): print(f'\\033[32magent-{row:03}\\033[0m')\n"
            f"Path({str(program_ready)!r}).write_text('ready')\n"
            f"Path({str(program_input)!r}).write_text(input())\n", encoding="utf-8")
        type_command(f"python3 {shlex.quote(str(program))}")
        wait_for_marker(program_ready, "ready")
        wheel(4, 30)
        key(window, "Home")
        key(window, "ctrl+shift+c")
        copied = subprocess.check_output(["xclip", "-selection", "clipboard", "-o"], timeout=10)
        assert b"agent-001" in copied.splitlines(), "foreground program scrollback is unreachable"
        if preview:
            subprocess.run(["scrot", "--overwrite", "--focused", str(Path(preview) / "agent-history.png")],
                           check=True, timeout=10)
        wheel(5, 260)
        time.sleep(0.25)
        wheel(4, 6)
        type_command("still-live")
        wait_for_marker(program_input, "still-live")

        # Exercise both input protocols in a real foreground PTY. Record only
        # the fixture's synthetic wheel bytes before restoring terminal modes.
        probe = root / "wheel-probe.py"
        probe.write_text(
            "import os, sys, termios, tty\nfrom pathlib import Path\n"
            "mode, ready, result = sys.argv[1:]\n"
            "saved = termios.tcgetattr(0)\ntty.setraw(0)\n"
            "try:\n"
            "    os.write(1, b'\\x1b[?1049h' + (b'\\x1b[?1000h\\x1b[?1006h' if mode == 'mouse' else b'\\x1b[?1h'))\n"
            "    Path(ready).write_text('ready')\n"
            "    data = b''\n"
            "    while b'!' not in data: data += os.read(0, 256)\n"
            "    Path(result).write_text(data.partition(b'!')[0].hex())\n"
            "finally:\n"
            "    os.write(1, b'\\x1b[?1000l\\x1b[?1006l\\x1b[?1l\\x1b[?1049l')\n"
            "    termios.tcsetattr(0, termios.TCSANOW, saved)\n", encoding="utf-8")
        for mode in ("cursor", "mouse"):
            ready = root / f"{mode}-ready"
            result = root / f"{mode}-wheel"
            type_command(shlex.join(["python3", str(probe), mode, str(ready), str(result)]))
            wait_for_marker(ready, "ready")
            time.sleep(0.3)  # Allow the new mode to reach the rendered frame.
            wheel(4, 1)
            wheel(5, 1)
            subprocess.run(["xdotool", "type", "--clearmodifiers", "!"], check=True, timeout=10)
            for _ in range(80):
                if result.exists() and result.read_text():
                    break
                time.sleep(0.1)
            data = bytes.fromhex(result.read_text())
            pattern = (rb"(?:\x1bOA)+(?:\x1bOB)+" if mode == "cursor" else
                       rb"(?:\x1b\[<64;\d+;\d+M)+(?:\x1b\[<65;\d+;\d+M)+")
            assert re.fullmatch(pattern, data), f"unexpected {mode} wheel input: {data!r}"

        # Turn off the tty's literal-next handling so the shell can observe Ctrl+V.
        chord_marker = root / "terminal-chords"
        type_command(
            'stty -iexten; corvid_fixture_ready chords; IFS= read -r line; stty iexten; '
            f'printf %s "$line" > {shlex.quote(str(chord_marker))}'
        )
        wait_for_marker(keyboard_ready, "chords")
        for chord in ("shift+Return", "alt+b", "ctrl+Home", "shift+F5", "ctrl+shift+x", "ctrl+v"):
            key(window, chord)
        key(window, "Return")
        wait_for_marker(chord_marker, "\x1b[13;2u\x1bb\x1b[1;5H\x1b[15;2~\x1b[120;6u\x16")

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
        second_terminal = wait_for_terminal(previous=first_terminal)
        second_tab_marker = root / "second-tab"
        write_marker(window, second_tab_marker,
                     f"second=preserved; printf %s \"$second\" > {shlex.quote(str(second_tab_marker))}")
        if args.build:
            key(window, "ctrl+shift+t")
            added_tab = wait_for_terminal(previous=second_terminal)
            key(window, "ctrl+shift+d")
            wait_for_terminal(previous=added_tab)
            if preview := os.environ.get("CORVID_PANE_CLOSE_PREVIEW_DIR"):
                subprocess.run(["scrot", "--overwrite", "--focused", str(Path(preview) / "pane-close-before.png")],
                               check=True, timeout=10)
            key(window, "ctrl+shift+w")
            wait_for_terminal(expected=added_tab)
            if preview:
                subprocess.run(["scrot", "--overwrite", "--focused", str(Path(preview) / "pane-close-after.png")],
                               check=True, timeout=10)
            key(window, "ctrl+shift+w")
            wait_for_terminal(expected=first_terminal)
            key(window, "ctrl+Tab")
            wait_for_terminal(expected=second_terminal)

            key(window, "ctrl+shift+d")
            split = wait_for_terminal(previous=second_terminal)

            def confirm_pane_close():
                subprocess.run(["xdotool", "mousemove", "--window", window, "1055", "20",
                                "click", "1"], check=True, timeout=10)
                # The last four items are close pane, close tab, close workspace, and Quit.
                time.sleep(0.2)
                subprocess.run(["xdotool", "key", "--repeat", "16", "Down"],
                               check=True, timeout=10)
                subprocess.run(["xdotool", "key", "Up", "Up", "Up", "Return"],
                               check=True, timeout=10)
                time.sleep(0.3)

            confirm_pane_close()
            if screenshot := os.environ.get("CORVID_CLOSE_SCREENSHOT"):
                subprocess.run(["scrot", "--overwrite", "--focused", screenshot],
                               check=True, timeout=10)
            key(window, "Escape")
            wait_for_terminal(expected=split)
            confirm_pane_close()
            key(window, "Return")
            wait_for_terminal(expected=second_terminal)
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
        if args.build:
            # The build-stage test exercises the folder picker while the
            # terminal window still owns focus. The archive smoke test covers
            # package startup and the terminal and Deck integration below.
            project = root / "project folder"
            project.mkdir()
            old_terminal = wait_for_terminal()
            key(window, "ctrl+shift+n")
            time.sleep(0.8)
            subprocess.run(["xdotool", "type", "--clearmodifiers", "--", "project"], check=True, timeout=10)
            time.sleep(0.5)
            key(window, "Tab")
            if screenshot := os.environ.get("CORVID_WORKSPACE_SCREENSHOT"):
                time.sleep(0.5)
                subprocess.run(["scrot", "--overwrite", "--focused", screenshot], check=True, timeout=10)
            key(window, "Return")
            wait_for_terminal(previous=old_terminal)
            workspace_marker = root / "workspace"
            cwd = shlex.quote(str(project.resolve()))
            marker = shlex.quote(str(workspace_marker))
            write_marker(window, workspace_marker, f'test "$(pwd -P)" = {cwd} && printf preserved > {marker}')

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

        if screenshot := os.environ.get("CORVID_AGENT_DECK_SETTINGS_SCREENSHOT"):
            geometry = subprocess.check_output(
                ["xdotool", "getwindowgeometry", "--shell", deck_window], text=True
            )
            width = int(re.search(r"^WIDTH=(\d+)$", geometry, re.M)[1])
            subprocess.run(["xdotool", "windowactivate", "--sync", deck_window,
                            "mousemove", "--window", deck_window, str(width - 53), "18", "click", "1"],
                           check=True, timeout=10)
            time.sleep(0.5)
            subprocess.run(["xdotool", "mousemove", "--window", deck_window, "95", "60", "click", "1",
                            "mousemove", "--window", deck_window, "390", "370"],
                           check=True, timeout=10)
            time.sleep(0.5)
            subprocess.run(["scrot", "--overwrite", "--focused", screenshot], check=True, timeout=10)

        print("PASS Desktop keyboard input, Unicode clipboard paste, tab creation/switching, independent windows, new-window menu, close/quit/relaunch, shell continuity, settings, and X11 icons")
    finally:
        for process in [deck, desktop, previous, daemon, manager]:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=15)
        monitor_fixture.stop(root / "corvid", monitor_state, environment)
