"""Exercise a live daemon through separate Linux client TTYs."""

import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty

PASTE_START = b"\x1b[200~"
PASTE_END = b"\x1b[201~"
PASTE = "한글".encode() + b"\x1dd\x1b[20X\n"
KEYS = b"\x1b\x1bx\x1b[A" + "한글".encode() + b"\x1d\x03"


def child(progress):
    tty.setraw(0)
    ticks = 0
    previous_size = os.get_terminal_size(0)
    while True:
        cols, rows = os.get_terminal_size(0)
        if (cols, rows) != previous_size:
            os.write(1, f"\x1b[5;1HRESIZED={cols}x{rows}".encode())
            previous_size = cols, rows
        status = f"\x1b[H\x1b[KPID={os.getpid()} TICK={ticks:06} SIZE={cols}x{rows}"
        os.write(1, status.encode())
        ticks += 1
        with progress.open("a") as log:
            log.write(f"{ticks} {cols}x{rows}\n")
        if select.select([0], [], [], 0.05)[0]:
            data = os.read(0, 4096)
            if b"a" in data:
                os.write(1, "\x1b[2;1H한글 e\u0301".encode())
            if b"s" in data:
                size = f"\x1b[3;1H\x1b[KMEASURED={cols}x{rows}"
                os.write(1, size.encode())
            if b"q" in data:
                os.write(1, b"\x1b[4;1HTERMINAL-EXIT")
                return 7


def read_exact(length):
    deadline = time.monotonic() + 5
    output = bytearray()
    while len(output) < length:
        wait = max(0, deadline - time.monotonic())
        assert select.select([0], [], [], wait)[0], output
        chunk = os.read(0, length - len(output))
        assert chunk, output
        output.extend(chunk)
    return output


def paste_child():
    tty.setraw(0)
    os.write(1, b"PLAIN-READY")
    assert read_exact(len(PASTE)) == PASTE
    os.write(1, b"\x1b[2;1H\x1b[?2004hBRACKET-READY")
    bracketed = PASTE_START + PASTE + PASTE_END
    assert read_exact(len(bracketed)) == bracketed
    os.write(1, b"\x1b[3;1HKEYS-READY")
    assert read_exact(len(KEYS)) == KEYS
    os.write(1, b"\x1b[4;1HINPUT-COMPLETE")
    return 0


class OuterTerminal:
    def __init__(self, command, cols=80, rows=24, env=None):
        self.master, self.slave = pty.openpty()
        self.original = termios.tcgetattr(self.slave)
        self.output = bytearray()
        self.resize(cols, rows)
        self.process = subprocess.Popen(
            command, stdin=self.slave, stdout=self.slave, stderr=self.slave, env=env,
        )

    def resize(self, cols, rows):
        size = struct.pack("HHHH", rows, cols, 0, 0)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, size)

    def expect(self, pattern):
        deadline = time.monotonic() + 5
        while (match := re.search(pattern, self.output)) is None:
            wait = max(0, deadline - time.monotonic())
            assert select.select([self.master], [], [], wait)[0], self.output[-500:]
            self.output.extend(os.read(self.master, 8192))
            assert len(self.output) <= 1024 * 1024, "unexpected output volume"
        return match

    def send(self, data):
        os.write(self.master, data)

    def wait(self, code):
        assert self.process.wait(timeout=5) == code, self.output[-500:]
        assert termios.tcgetattr(self.slave) == self.original, "outer TTY was not restored"

    def __enter__(self):
        return self

    def __exit__(self, *_):
        stop(self.process)
        os.close(self.master)
        os.close(self.slave)


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=2)


def wait_for_daemon(command, process):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        assert process.poll() is None, "daemon exited during startup"
        ping = subprocess.run([*command, "control", "ping"], capture_output=True, timeout=2)
        if ping.returncode == 0:
            return
        time.sleep(0.02)
    raise AssertionError("daemon did not become ready")


def verify(command, directory):
    with OuterTerminal([*command, "attach"]) as empty:
        empty.expect(b"NoTerminal")
        empty.wait(1)

    progress = directory / "child-progress"
    start = [*command, "attach", "--", sys.executable, __file__, "--child", progress]
    with OuterTerminal(start) as first:
        initial = first.expect(rb"PID=(\d+) TICK=(\d+) SIZE=80x24")
        pid, tick = int(initial[1]), int(initial[2])
        first.send(b"a")
        first.expect("한글 e\u0301".encode())
        verify_pairing(command, first, pid)

        with OuterTerminal([*command, "attach"], cols=110, rows=31) as busy:
            busy.expect(b"WorkspaceBusy")
            busy.wait(1)
        first.send(b"s")
        first.expect(b"MEASURED=80x24")

        first.resize(100, 30)
        first.expect(b"RESIZED=100x30")
        first.send(b"\x1dd")
        first.wait(0)

    ticks_at_detach = len(progress.read_text().splitlines())
    time.sleep(0.3)
    assert len(progress.read_text().splitlines()) > ticks_at_detach
    with OuterTerminal([*command, "attach"], cols=100, rows=30) as second:
        retained = second.expect(rb"PID=(\d+) TICK=(\d+) SIZE=100x30")
        assert int(retained[1]) == pid
        assert int(retained[2]) > tick
        second.expect("한글 e\u0301".encode())
        second.send(b"q")
        second.expect(b"TERMINAL-EXIT")
        second.wait(7)


def verify_pairing(command, outer, pid):
    outer.send(b"\x1dr")
    outer.expect(b"Controller pairing code")
    code = outer.expect(rb"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}")[0].decode()
    host = subprocess.check_output([*command, "control", "host"], text=True, timeout=3)
    identity = dict(field.split("=", 1) for field in host.split())
    paired = subprocess.run(
        [*command, "controller", "--controller-id", "tty-fixture", "--host-id", identity["host_id"],
         "--server-epoch", identity["server_epoch"], "pair"],
        input=code + "\n", text=True, capture_output=True, timeout=3, check=True,
    )
    assert "binding_revision=1" in paired.stdout
    clients = subprocess.check_output([*command, "control", "clients"], text=True, timeout=3)
    assert "binding=tty-fixture/1" in clients and code not in clients
    outer.output.clear()
    outer.send(b"\r")
    restored = outer.expect(rb"PID=(\d+) TICK=\d+ SIZE=80x24")
    assert int(restored[1]) == pid


def send_paste(outer):
    for chunk in (PASTE_START[:4], PASTE_START[4:], PASTE, PASTE_END[:2], PASTE_END[2:]):
        outer.send(chunk)
        time.sleep(0.01)


def verify_paste(command):
    start = [*command, "attach", "--", sys.executable, __file__, "--paste-child"]
    with OuterTerminal(start) as outer:
        outer.expect(b"PLAIN-READY")
        send_paste(outer)
        outer.expect(b"BRACKET-READY")
        send_paste(outer)
        outer.expect(b"KEYS-READY")

        outer.send(b"\x1b")
        time.sleep(0.15)
        for chunk in (b"\x1b", b"x", b"\x1b[", b"A", *[bytes([byte]) for byte in "한글".encode()], b"\x1d\x1d", b"\x03"):
            outer.send(chunk)
            time.sleep(0.01)
        outer.expect(b"INPUT-COMPLETE")
        outer.wait(0)


def verify_tabs(command, directory):
    attach = [*command, "attach"]
    fixture = [sys.executable, __file__, "--child"]
    marker = rb"PID=(\d+) TICK=\d+ SIZE=80x24"
    with OuterTerminal([*attach, "--", *fixture, directory / "first-tab"]) as first:
        first_pid = first.expect(marker)[1]
        first.send(b"\x1dd")
        first.wait(0)

    time.sleep(0.05)
    second_log = directory / "second-tab"
    with OuterTerminal([*attach, "--new-tab", "Second", "--", *fixture, second_log]) as second:
        second_pid = second.expect(marker)[1]
        assert second_pid != first_pid
        second.output.clear()
        second.send(b"\x1dp")
        assert second.expect(marker)[1] == first_pid
        second.resize(100, 30)
        second.expect(b"RESIZED=100x30")
        deadline = time.monotonic() + 5
        while not second_log.read_text().strip().endswith("100x30"):
            assert time.monotonic() < deadline, "hidden tab kept the old PTY size"
            time.sleep(0.02)
        second.output.clear()
        second.send(b"\x1dn")
        assert second.expect(rb"PID=(\d+) TICK=\d+ SIZE=100x30")[1] == second_pid
        second.send(b"\x1dd")
        second.wait(0)

    verify_tab_move(command, directory, first_pid, second_pid)


def verify_tab_move(command, directory, moved_pid, visible_pid):
    attach = [*command, "attach"]
    marker = rb"PID=(\d+) TICK=\d+ SIZE=100x30"
    start = [
        *attach, "--new-workspace", "Destination", "--", sys.executable,
        __file__, "--child", directory / "destination-tab",
    ]
    with OuterTerminal(start) as destination:
        destination.expect(rb"PID=\d+ TICK=\d+ SIZE=80x24")
        destination.send(b"\x1dd")
        destination.wait(0)

    def inventory():
        return subprocess.check_output([*command, "control", "workspaces"], text=True, timeout=3)

    layout = inventory()
    source_id = re.search(r"^workspace ([\da-f-]+) Main$", layout, re.M)[1]
    destination_id = re.search(r"^workspace ([\da-f-]+) Destination$", layout, re.M)[1]
    moved_tab = re.search(r"^  tab ([\da-f-]+) Shell$", layout, re.M)[1]
    visible_tab = re.search(r"^  tab ([\da-f-]+) Second$", layout, re.M)[1]
    move = [
        *attach, "--workspace", source_id, "--tab", visible_tab,
        "--claim-workspace", destination_id, "--move-tab", moved_tab, "--to-workspace", destination_id,
    ]
    with OuterTerminal(move, cols=100, rows=30) as visible:
        assert visible.expect(marker)[1] == visible_pid
        deadline = time.monotonic() + 5
        while f"  tab {moved_tab} Shell" not in inventory().split(" Destination\n", 1)[1]:
            assert time.monotonic() < deadline, "tab did not move to the destination"
            time.sleep(0.02)
        visible.send(b"\x1dd")
        visible.wait(0)

    with OuterTerminal([*attach, "--workspace", destination_id, "--tab", moved_tab],
                       cols=100, rows=30) as moved:
        assert moved.expect(marker)[1] == moved_pid
        moved.send(b"\x1dd")
        moved.wait(0)


def background_daemon_pid(runtime):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.connect(os.fspath(runtime / "control.sock"))
        credentials = connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
    pid, uid, _ = struct.unpack("3i", credentials)
    assert uid == os.getuid()
    return pid


def verify_default_start(binary, directory):
    runtime = directory / "auto"
    home = directory / "home"
    home.mkdir()
    env = os.environ | {
        "HOME": str(home), "SHELL": "/bin/sh", "PS1": "CORVID-PROMPT> ",
        "XDG_STATE_HOME": str(home),
    }
    env.pop("TERMINFO", None)
    env.pop("TERMINFO_DIRS", None)
    command = [binary, "--runtime-dir", runtime]
    for args in ([], ["control", "ping"]):
        result = subprocess.run(
            [*command, *args], stdin=subprocess.DEVNULL, capture_output=True, env=env, timeout=5,
        )
        assert result.returncode != 0 and not runtime.exists(), result

    def shell_pid(terminal):
        terminal.expect(b"CORVID-PROMPT>")
        terminal.send(b"infocmp corvid >/dev/null && printf 'CORVID-SHELL=%s\\n' \"$$\"\n")
        return terminal.expect(rb"CORVID-SHELL=(\d+)")[1]

    try:
        with OuterTerminal(command, env=env) as first, OuterTerminal(command, env=env) as second:
            sessions = [first, second]
            startup = rb"CORVID-PROMPT>|WorkspaceBusy"
            active, = [
                terminal for terminal in sessions
                if terminal.expect(startup)[0] == b"CORVID-PROMPT>"
            ]
            rejected, = [terminal for terminal in sessions if terminal is not active]
            rejected.wait(1)
            pid = shell_pid(active)
            daemon = background_daemon_pid(runtime)
            assert os.getsid(daemon) == daemon
            active.send(b"\x1dd")
            active.wait(0)

        with OuterTerminal(command, env=env) as second:
            assert shell_pid(second) == pid
            assert background_daemon_pid(runtime) == daemon
            second.send(b"exit\n")
            second.wait(0)
    finally:
        try:
            daemon = background_daemon_pid(runtime)
        except (FileNotFoundError, ConnectionRefusedError):
            pass
        else:
            handle = os.pidfd_open(daemon)
            try:
                signal.pidfd_send_signal(handle, signal.SIGTERM)
                assert select.select([handle], [], [], 5)[0], "background daemon did not stop"
            finally:
                os.close(handle)


def run(binary):
    root = Path(__file__).resolve().parents[3]
    profile = root / "spikes/terminal/corvid.terminfo"
    with tempfile.TemporaryDirectory(prefix="corvid-test-", dir=root) as temporary:
        directory = Path(temporary)
        database = directory / "terminfo"
        subprocess.run(["tic", "-x", "-o", database, profile], check=True)
        command = [binary, "--runtime-dir", directory / "runtime"]
        daemon = subprocess.Popen(
            [*command, "daemon", "--state-dir", directory / "state"],
            env=os.environ | {"TERMINFO": str(database)},
        )
        try:
            wait_for_daemon(command, daemon)
            verify(command, directory)
            verify_paste(command)
            verify_tabs(command, directory)
        finally:
            stop(daemon)
        verify_default_start(binary, directory)
    print("Live TTYs verified default startup, writer fencing, redraw, paste modes, fragmented keys, detach, and tab moves.")


if __name__ == "__main__":
    if sys.argv[1] == "--child":
        sys.exit(child(Path(sys.argv[2])))
    if sys.argv[1] == "--paste-child":
        sys.exit(paste_child())
    run(sys.argv[1])
