#!/usr/bin/env python3
"""Click Agent Deck cards and verify Desktop focus on an isolated X11 display."""
import argparse
from contextlib import ExitStack
import fcntl
import os
from pathlib import Path
import pty
import signal
import struct
import subprocess
import termios
import tempfile
import time

import monitor_fixture

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


def foreground():
    result = run("xdotool", "getactivewindow", capture_output=True, text=True)
    return result.stdout.strip()


def click(handle, x, y):
    run("xdotool", "mousemove", "--window", handle, str(x), str(y), "click", "1")


def geometry(handle):
    return dict(line.split("=", 1) for line in run(
        "xdotool", "getwindowgeometry", "--shell", handle, capture_output=True, text=True
    ).stdout.splitlines() if "=" in line)


def launch_agent(corvid, env, root, name):
    pid, master = pty.fork()
    if pid == 0:
        os.chdir(root)
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        os.execve(corvid, [corvid, "attach", "--new-tab", name, "--agent", "claude", "--", name], env)
    os.set_blocking(master, False)
    output = bytearray()

    def ready():
        try:
            output.extend(os.read(master, 16384))
        except BlockingIOError:
            pass
        except OSError as error:
            raise AssertionError(f"Agent {name} did not attach: {output[-1500:]!r}") from error
        return (root / f"ready-{name}").exists()

    try:
        until(f"agent {name}", ready)
    finally:
        os.close(master)
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)


def main(build):
    with ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix=".deck-focus-", dir=Path.home())))
        runtime = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix=".cvf-", dir=Path.home())))
        corvid = str(build / "corvid")
        env = dict(os.environ, HOME=str(root), CORVID_RUNTIME_DIR=str(runtime / "corvid"),
                   CORVID_VIEW_STATE_DIR=str(root / "views"), XDG_STATE_HOME=str(root / "state"),
                   SHELL="/bin/sh", TERM="xterm-256color", XDG_RUNTIME_DIR=str(runtime),
                   CORVID_FIXTURE_BINARY=corvid,
                   CORVID_FIXTURE_DIR=str(root), PATH=str(root) + os.pathsep + os.environ["PATH"])
        provider = root / "claude"
        fixture = Path(__file__).with_name("fixtures") / "deck_focus_agent.rs"
        run("rustc", "--edition=2024", str(fixture), "-o", str(provider))
        state = root / "state/corvid/companion"
        daemon = processes(stack, corvid, "daemon", env=env)
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
        def slots():
            return run(*reader, "slots", env=env, capture_output=True, text=True).stdout

        try:
            until("two agent cards", lambda: slots().count("availability=Available") == 2)
        except AssertionError as error:
            agents = run(corvid, "control", "agents", env=env, capture_output=True, text=True).stdout
            raise AssertionError(f"{error}\n{slots()}\n{agents}") from error
        run("xdotool", "windowactivate", "--sync", desktop)
        click(desktop, int(geometry(desktop)["WIDTH"]) - 65, 20)
        deck = until("Agent Deck opened from Desktop", lambda: window("Corvid Agent Deck"))

        for index, name in enumerate(("second", "first")):
            if index == 1:
                run("xdotool", "windowminimize", "--sync", desktop)
            run("xdotool", "windowactivate", "--sync", deck)
            time.sleep(0.5)
            click(deck, 60 + (1 - index) * 94, 80)
            until(f"foreground Desktop for {name}", lambda: foreground() == desktop)
            token = f"deck-focus-{name}"
            run("xdotool", "type", "--clearmodifiers", token)
            run("xdotool", "key", "Return")
            marker = root / f"input-{name}"
            until(f"input in {name}'s pane", lambda: marker.exists() and marker.read_text() == token)
        print("PASS Deck clicks focused two distinct agent panes, including a minimized Desktop")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path, required=True)
    main(parser.parse_args().build.resolve())
