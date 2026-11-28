#!/usr/bin/env python3
"""Click Agent Deck cards and verify Desktop focus on an isolated X11 display."""
import argparse
from contextlib import ExitStack
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

REPOSITORY = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPOSITORY / "tests/ui"))
import monitor_fixture
from deck_focus_fixture import agent_environment, launch_agent

WAIT = 15


def until(description, check):
    deadline = time.monotonic() + WAIT
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"Timed out waiting for {description}")


def run(*args, **kwargs):
    return subprocess.run(args, check=True, timeout=8, **kwargs)


def processes(stack, *args, **kwargs):
    child = subprocess.Popen(args, start_new_session=True, **kwargs)

    def stop():
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
            child.wait(timeout=8)

    stack.callback(stop)
    return child


def window(title):
    result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", f"^{title}$"],
                            capture_output=True, text=True, timeout=5)
    return result.stdout.splitlines()[0] if result.returncode == 0 else None


def has_keyboard_focus(handle):
    for query in ("getactivewindow", "getwindowfocus"):
        result = run("xdotool", query, capture_output=True, text=True)
        if result.stdout.strip() != handle:
            return False
    return True


def click(handle, x, y):
    run("xdotool", "mousemove", "--window", handle, str(x), str(y), "click", "1")


def deck_pixels(frame):
    run("scrot", "--overwrite", "--focused", str(frame))
    # scrot writes a binary RGB PPM and may include a comment before its dimensions.
    header, pixels = frame.read_bytes().split(b"\n255\n", 1)
    width, _ = map(int, header.splitlines()[-1].split())
    return width, pixels


def key_colors(frame, expected):
    width, pixels = deck_pixels(frame)
    scale = width / 392
    row = round(84 * scale) * width * 3
    for index, color in enumerate(expected):
        left = round((12 + 94 * index) * scale) * 3
        right = round((102 + 94 * index) * scale) * 3
        if bytes.fromhex(color) not in pixels[row + left:row + right]:
            return False
    return True


def tooltip_color(frame, expected):
    width, pixels = deck_pixels(frame)
    scale = width / 392
    # The first key's tooltip opens at the pointer. Its status text crosses
    # the otherwise empty gap between the first two cards.
    left, right = round(98 * scale), round(108 * scale)
    color = bytes.fromhex(expected)
    for y in range(round(30 * scale), round(80 * scale)):
        for x in range(left, right):
            offset = (y * width + x) * 3
            pixel = pixels[offset:offset + 3]
            if (pixel[1] > pixel[2]) == (color[1] > color[2]) and all(
                    abs(actual - target) <= 40 for actual, target in zip(pixel, color)):
                return True
    return False


def geometry(handle):
    return dict(line.split("=", 1) for line in run(
        "xdotool", "getwindowgeometry", "--shell", handle, capture_output=True, text=True
    ).stdout.splitlines() if "=" in line)


def open_deck(desktop):
    run("xdotool", "windowactivate", "--sync", desktop)
    # Give the overlay time to process the shortcut and filter before submitting.
    run("xdotool", "key", "--clearmodifiers", "--delay", "150", "ctrl+bracketright", "question")
    run("xdotool", "type", "--clearmodifiers", "--delay", "100", "Agents")
    time.sleep(0.2)
    run("xdotool", "key", "--clearmodifiers", "Return")
    return until("Agent Deck opened from Desktop", lambda: window("Corvid Agent Deck"))


def fields(line):
    return dict(field.split("=", 1) for field in line.split() if "=" in field)


def recover_agent_cards(stack, daemon, corvid, env, runtime, agents, slots, control):
    sys.path.insert(0, str(REPOSITORY / "crates/corvid-tui/tests"))
    from agent_handoff import prepare

    previous = {fields(line)["run"] for line in slots().splitlines()
                if line.startswith("slot=") and "availability=Available" in line}
    assert len(previous) == 2, previous
    identity = prepare([corvid, "--runtime-dir", runtime], runtime,
                       REPOSITORY / "crates/corvid-protocol/proto/corvid.proto")
    assert daemon.wait(timeout=5) == 0
    processes(stack, corvid, "daemon", "--autostarted", env=env)
    until("replacement daemon", lambda: subprocess.run(
        [corvid, "control", "ping"], env=env, capture_output=True, timeout=5
    ).returncode == 0)

    def recovered():
        header, *lines = slots().splitlines()
        header = fields(header)
        if header.get("connection") != "Ready" or header.get("remote_epoch") == identity["server_epoch"]:
            return None
        assert header["host_id"] == identity["host_id"], header
        cards = [fields(line) for line in lines if line.startswith("slot=") and " run=" in line]
        history = {fields(line)["run"] for line in lines if line.startswith("ended ")}
        if len(cards) != 2 or not previous <= history:
            return None
        assert all(card["availability"] == "Available" for card in cards), cards
        assert previous.isdisjoint(card["run"] for card in cards), cards
        return {card["run"].rsplit("/", 1)[-1]: int(card["slot"]) - 1 for card in cards}

    indices = until("two recovered cards and both prior runs in history", recovered)
    old_agents = {agent["session"]: (agent["pid"], agent["terminal"]) for agent in agents}
    current = {fields(line)["session"]: (line.split()[0], fields(line))
               for line in control("agents").splitlines()}
    assert {name: (agent["pid"], agent["terminal"]) for name, (_, agent) in current.items()} == old_agents
    return {name: indices[run_id] for name, (run_id, _) in current.items()}


def capture_history(deck, scale, screenshot):
    run("xdotool", "windowactivate", "--sync", deck)
    width = int(geometry(deck)["WIDTH"])
    click(deck, width - round(57 * scale), round(18 * scale))
    until("expanded Deck", lambda: int(geometry(deck)["WIDTH"]) > width)
    click(deck, round(197 * scale), round(153 * scale))
    run("xdotool", "mousemove", "--window", deck, str(round(246 * scale)), str(round(176 * scale)))
    time.sleep(0.2)
    run("scrot", "--overwrite", "--focused", str(screenshot))


def main(build, screenshot, history_screenshot, tooltip_screenshot, failure_screenshot):
    with ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix=".deck-focus-", dir=Path.home())))
        runtime = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix=".cvf-", dir=Path.home())))
        corvid = str(build / "corvid")
        env = dict(os.environ, HOME=str(root), CORVID_RUNTIME_DIR=str(runtime / "corvid"),
                   CORVID_VIEW_STATE_DIR=str(root / "views"), XDG_STATE_HOME=str(root / "state"),
                   SHELL="/bin/sh", TERM="xterm-256color", XDG_RUNTIME_DIR=str(runtime))
        env = agent_environment(corvid, root, env)
        state = root / "state/corvid/companion"
        daemon = processes(stack, corvid, "daemon", "--autostarted", env=env)
        processes(stack, "openbox", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        until("daemon", lambda: subprocess.run(
            [corvid, "control", "ping"], env=env, capture_output=True, timeout=5
        ).returncode == 0 and daemon.poll() is None)
        until("window manager", lambda: "_NET_ACTIVE_WINDOW" in run(
            "xprop", "-root", "_NET_SUPPORTED", capture_output=True, text=True
        ).stdout)
        for name in ("first", "second"):
            launch_agent(corvid, env, root, name)
        processes(stack, str(build / "corvid-desktop"), env=env)
        desktop = until("Desktop window", lambda: window("Corvid Desktop"))
        reader, _ = monitor_fixture.ready(corvid, state, env)
        stack.callback(monitor_fixture.stop, corvid, state, env)

        def capture_failure(exc_type, _value, _traceback):
            if exc_type is not None and failure_screenshot:
                try:
                    run("scrot", "--overwrite", "--focused", failure_screenshot)
                except (OSError, subprocess.SubprocessError) as error:
                    print(f"Could not capture fixture window: {error}")

        stack.push(capture_failure)

        def control(command):
            return run(corvid, "control", command, env=env, capture_output=True, text=True).stdout

        def slots():
            return run(*reader, "slots", env=env, capture_output=True, text=True).stdout

        try:
            until("two agent cards", lambda: slots().count("availability=Available") == 2)
        except AssertionError as error:
            raise AssertionError(f"{error}\n{slots()}\n{control('agents')}") from error
        agents = [fields(line) for line in control("agents").splitlines()]
        terminals = {agent["session"]: agent["terminal"] for agent in agents}
        until("Desktop pane writer", lambda: "pane-writer=" in control("clients"))
        deck = open_deck(desktop)
        run("xdotool", "windowactivate", "--sync", deck)
        scale = int(geometry(deck)["WIDTH"]) / 392
        frame = root / "deck.ppm"
        blue, gray, mint = "8bb8e8", "8b8b95", "6bc48d"
        until("submitted and idle keys", lambda: key_colors(frame, (blue, gray)))

        for index, name in enumerate(("second", "first")):
            if index == 1:
                run("xdotool", "windowminimize", "--sync", desktop)
                until("minimized Desktop", lambda: "Iconic" in run(
                    "xprop", "-id", desktop, "WM_STATE", capture_output=True, text=True
                ).stdout)
            run("xdotool", "windowactivate", "--sync", deck)
            time.sleep(0.5)
            click(deck, round((55 + (1 - index) * 94) * scale), round(84 * scale))
            try:
                until(f"Desktop keyboard focus for {name}", lambda: has_keyboard_focus(desktop))
                until(f"Desktop writer for {name}'s pane", lambda: any(
                    f"terminal={terminals[name]}" in line.split() and "pane-writer=" in line
                    for line in control("clients").splitlines()
                ))
                # A restored window can resize the newly selected terminal. Wait for
                # its full frame, as in the Desktop keyboard-input smoke test.
                time.sleep(0.5)
                token = f"deck-focus-{name}"
                # Toolbar discovery can send Escape to the initial pane. Clear
                # the fixture's canonical input line before typing its marker.
                run("xdotool", "key", "--clearmodifiers", "ctrl+u")
                run("xdotool", "type", "--delay", "20", "--clearmodifiers", token)
                run("xdotool", "key", "Return")
                marker = root / f"input-{name}"
                until(f"input in {name}'s pane", lambda: marker.exists() and marker.read_text() == token)
                run("xdotool", "windowactivate", "--sync", deck)
                if name == "first":
                    until("open working tooltip", lambda: tooltip_color(frame, blue))
                    (root / "finish-turn").touch()
                    until("same open tooltip showing a response", lambda: tooltip_color(frame, mint))
                    if tooltip_screenshot:
                        run("scrot", "--overwrite", "--focused", str(tooltip_screenshot))
                run("xdotool", "mousemove", "--window", deck, str(round(200 * scale)), str(round(20 * scale)))
                colors = (blue, blue) if index == 0 else (mint, blue)
                until(f"live prompt status for {name}", lambda: key_colors(frame, colors))
            except AssertionError as error:
                if screenshot:
                    run("scrot", "--overwrite", "--focused", str(screenshot))
                markers = {path.name: path.read_text() for path in root.glob("input-*")}
                raise AssertionError(f"{error}\n{control('clients')}\n{slots()}\nFixture input: {markers!r}") from error
        if screenshot:
            run("scrot", "--overwrite", "--focused", str(screenshot))

        recovered = recover_agent_cards(stack, daemon, corvid, env, runtime / "corvid", agents, slots, control)
        for name in ("second", "first"):
            def focus_recovered():
                run("xdotool", "windowactivate", "--sync", deck)
                click(deck, round((55 + recovered[name] * 94) * scale), round(84 * scale))
                time.sleep(0.3)
                return has_keyboard_focus(desktop) and any(
                    f"terminal={terminals[name]}" in line.split() and "pane-writer=" in line
                    for line in control("clients").splitlines()
                )

            until(f"Deck click focused {name}'s retained terminal", focus_recovered)
        if history_screenshot:
            capture_history(deck, scale, history_screenshot)
        print("PASS Deck followed live prompts and focused both retained agents after daemon handoff")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path, required=True)
    parser.add_argument("--screenshot", type=Path)
    parser.add_argument("--history-screenshot", type=Path)
    parser.add_argument("--tooltip-screenshot", type=Path)
    parser.add_argument("--failure-screenshot", type=Path)
    args = parser.parse_args()
    main(args.build.resolve(), args.screenshot, args.history_screenshot, args.tooltip_screenshot,
         args.failure_screenshot)
