"""Start real managed terminals backed by the synthetic Claude hook producer."""
import errno
import fcntl
import os
from pathlib import Path
import pty
import signal
import struct
import subprocess
import termios
import time


def agent_environment(cli, root, env):
    fixture = Path(__file__).with_name("fixtures") / "deck_focus_agent.rs"
    subprocess.run(["rustc", "--edition=2024", fixture, "-o", root / "claude"],
                   check=True, timeout=30)
    return dict(env, CORVID_FIXTURE_BINARY=str(cli), CORVID_FIXTURE_DIR=str(root),
                PATH=str(root) + os.pathsep + env["PATH"])


def launch_agent(cli, env, root, name):
    title = {"first": "SSH reconnect review", "second": "Release checklist"}[name]
    pid, master = pty.fork()
    if pid == 0:
        os.chdir(root)
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        os.execve(cli, [str(cli), "attach", "--new-tab", title, "--agent", "claude", "--", name], env)
    os.set_blocking(master, False)
    output = bytearray()
    deadline = time.monotonic() + 15
    try:
        marker = root / f"ready-{name}"
        while time.monotonic() < deadline:
            if marker.exists():
                return
            try:
                chunk = os.read(master, 16384)
            except BlockingIOError:
                chunk = None
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                chunk = b""
            if chunk:
                output.extend(chunk)
            elif chunk == b"":
                break
            time.sleep(0.1)
        raise AssertionError(f"Agent {name} did not attach: {output[-1500:]!r}")
    finally:
        os.close(master)
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
