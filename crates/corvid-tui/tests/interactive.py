"""Exercise a live daemon through separate Unix client TTYs."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
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
PROVIDER_RELEASES = {
    "baseline": {},
    "older": {"CLAUDE": "2.1.300", "CODEX": "0.166.0"},
    "newer": {"CLAUDE": "2.1.325", "CODEX": "0.168.0"},
}


def terminal_attributes(fd):
    attributes = termios.tcgetattr(fd)
    if sys.platform == "darwin":
        # Darwin sets PENDIN when canonical mode is restored. It is pending-input
        # state, not a mode setting: xnu/bsd/kern/tty.c handles it in TIOCSETA.
        attributes[3] &= ~termios.PENDIN
    return attributes


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
    plain = read_exact(len(PASTE))
    assert plain == PASTE, ("plain paste", plain)
    os.write(1, b"\x1b[2;1H\x1b[?2004hBRACKET-READY")
    bracketed = PASTE_START + PASTE + PASTE_END
    received = read_exact(len(bracketed))
    assert received == bracketed, ("bracketed paste", received)
    os.write(1, b"\x1b[3;1HKEYS-READY")
    assert read_exact(len(KEYS)) == KEYS
    os.write(1, b"\x1b[4;1HINPUT-COMPLETE")
    return 0


def mouse_child(output):
    attributes = termios.tcgetattr(0)
    try:
        tty.setraw(0)
        os.write(1, b"\x1b[?1000h\x1b[?1006hEDITOR-READY")
        output.write_bytes(read_exact(18))
        os.write(1, b"\r\nAPP-MOUSE-RECEIVED")
    finally:
        os.write(1, b"\x1b[?1000l\x1b[?1006l")
        termios.tcsetattr(0, termios.TCSANOW, attributes)


class OuterTerminal:
    def __init__(self, command, cols=80, rows=24, env=None):
        self.master, self.slave = pty.openpty()
        self.original = terminal_attributes(self.slave)
        self.output = bytearray()
        self.resize(cols, rows)
        env = dict(os.environ if env is None else env)
        # Dedicated monitor/SSH fixtures exercise background monitoring and clean it up.
        env.setdefault("CORVID_AGENT_MONITOR", "0")
        env.setdefault("CORVID_LEGACY_TUI", "1")
        # Crossterm must measure this PTY, not an inherited terminal's dimensions.
        env.pop("COLUMNS", None)
        env.pop("LINES", None)
        self.process = subprocess.Popen(
            command, stdin=self.slave, stdout=self.slave, stderr=self.slave, env=env,
            start_new_session=True,
        )

    def resize(self, cols, rows):
        size = struct.pack("HHHH", rows, cols, 0, 0)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, size)

    def read_output(self, timeout):
        if not select.select([self.master], [], [], timeout)[0]:
            return False
        self.output.extend(os.read(self.master, 8192))
        return True

    def expect(self, pattern):
        deadline = time.monotonic() + 5
        while (match := re.search(pattern, self.output)) is None:
            wait = max(0, deadline - time.monotonic())
            received = self.read_output(wait)
            assert received, (
                f"outer TTY status: {self.process.poll()}", self.output[:700], self.output[-500:]
            )
            assert len(self.output) <= 1024 * 1024, "unexpected output volume"
        return match

    def send(self, data):
        os.write(self.master, data)

    def wait(self, code):
        deadline = time.monotonic() + 5
        # A child may still be writing a redraw or its TTY restoration sequence.
        # Keep reading the PTY until it exits so an unread buffer cannot trap it.
        while (status := self.process.poll()) is None:
            remaining = deadline - time.monotonic()
            assert remaining > 0, ("outer TTY process did not exit", self.output[-500:])
            self.read_output(min(remaining, 0.05))
            if len(self.output) > 1024 * 1024:
                del self.output[:-500]
        assert status == code, (status, self.output[-500:])
        restored = terminal_attributes(self.slave)
        assert restored == self.original, ("outer TTY was not restored", self.original, restored)

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


def verify_resize(outer, progress, cols, rows, inner_size):
    outer.resize(cols, rows)
    footer = f"\x1b[{rows};1H\x1b[0;2m\x1b[2KCORVID".encode()
    outer.expect(re.escape(footer))
    wait_for_child_size(progress, inner_size)


def wait_for_child_size(progress, size):
    deadline = time.monotonic() + 5
    while not progress.read_text().endswith(f" {size}\n"):
        assert time.monotonic() < deadline, f"child never reached {size}"
        time.sleep(0.02)


def verify(command, directory):
    with OuterTerminal([*command, "attach"]) as empty:
        empty.expect(b"NoTerminal")
        empty.wait(1)

    progress = directory / "child-progress"
    start = [*command, "attach", "--", sys.executable, __file__, "--child", progress]
    with OuterTerminal(start) as first:
        initial = first.expect(rb"PID=(\d+) TICK=(\d+) SIZE=80x23")
        first.expect(re.escape(b"\x1b[24;1H\x1b[0;2m\x1b[2KCORVID  Main"))
        pid, tick = int(initial[1]), int(initial[2])
        first.send(b"a")
        first.expect("한글 e\u0301".encode())
        verify_pairing(command, first, pid)
        verify_command_menu(first, progress, pid)

        with OuterTerminal([*command, "attach", "--new"], cols=110, rows=31) as busy:
            busy.expect(b"WorkspaceBusy")
            busy.wait(1)
        first.send(b"s")
        first.expect(b"MEASURED=80x23")

        verify_resize(first, progress, 100, 30, "100x29")
        verify_resize(first, progress, 319, 80, "319x79")
        verify_resize(first, progress, 500, 100, "500x72")
        first.expect(re.escape(b"\x1b[1;72r"))
        verify_resize(first, progress, 1200, 81, "1024x35")
        first.send(b"\x1dd")
        first.wait(0)

    ticks_at_detach = len(progress.read_text().splitlines())
    time.sleep(0.3)
    assert len(progress.read_text().splitlines()) > ticks_at_detach
    with OuterTerminal([*command, "attach"]) as interrupted:
        resumed = interrupted.expect(rb"PID=(\d+) TICK=\d+ SIZE=\d+x\d+")
        assert int(resumed[1]) == pid
        interrupted.process.send_signal(signal.SIGTERM)
        interrupted.wait(0)

    deadline = time.monotonic() + 5
    while b"clients=0" not in subprocess.check_output(
        [*command, "control", "snapshot"], timeout=3
    ):
        assert time.monotonic() < deadline, "terminated TUI still owns a client route"
        time.sleep(0.02)

    with OuterTerminal([*command, "attach"], cols=319, rows=80) as second:
        retained = second.expect(rb"PID=(\d+) TICK=(\d+) SIZE=\d+x\d+")
        assert int(retained[1]) == pid
        assert int(retained[2]) > tick
        wait_for_child_size(progress, "319x79")
        snapshot = subprocess.check_output([*command, "control", "snapshot"], timeout=3)
        assert b"clients=1" in snapshot
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
    restored = outer.expect(rb"PID=(\d+) TICK=\d+ SIZE=80x23")
    assert int(restored[1]) == pid


def verify_command_menu(outer, progress, pid):
    outer.output.clear()
    outer.send(b"\x1d?")
    outer.expect(b"CORVID COMMANDS")
    outer.send(b"QQQ" + PASTE_START + b"q\n" + PASTE_END)
    time.sleep(2.2)
    outer.output.clear()
    outer.send(b"?")
    outer.expect(b"CORVID COMMANDS")
    outer.output.clear()
    outer.send(b"\x1b")
    restored = outer.expect(rb"PID=(\d+) TICK=\d+ SIZE=80x23")
    assert int(restored[1]) == pid
    outer.expect(re.escape(b"CORVID  Main"))
    wait_for_child_size(progress, "80x23")


def send_paste(outer, *, opener_delay=0.01):
    outer.send(PASTE_START[:4])
    time.sleep(opener_delay)
    for chunk in (PASTE_START[4:], PASTE, PASTE_END[:2], PASTE_END[2:]):
        outer.send(chunk)
        time.sleep(0.01)


def verify_paste(command):
    start = [*command, "attach", "--", sys.executable, __file__, "--paste-child"]
    with OuterTerminal(start) as outer:
        outer.expect(b"PLAIN-READY")
        # The opener must survive a gap longer than the 100 ms Esc key delay.
        send_paste(outer, opener_delay=0.25)
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
    marker = rb"PID=(\d+) TICK=\d+ SIZE=80x23"
    with OuterTerminal([*attach, "--", *fixture, directory / "first-tab"]) as first:
        first_pid = first.expect(marker)[1]
        first.send(b"\x1dd")
        first.wait(0)

    time.sleep(0.05)
    second_log = directory / "second-tab"
    with OuterTerminal([*attach, "--new-tab", "Second", "--", *fixture, second_log]) as second:
        second_pid = second.expect(marker)[1]
        second.expect(re.escape(b"[2:Second]"))
        assert second_pid != first_pid
        second.output.clear()
        second.send(b"\x1d1")
        assert second.expect(marker)[1] == first_pid
        second.expect(re.escape(b"[1:Pane 1]"))
        second.resize(100, 30)
        second.expect(b"RESIZED=100x29")
        # Pane writers resize their selected pane; hidden panes retain their size
        # until selected, so another writer's viewport cannot be changed.
        assert second_log.read_text().strip().endswith("80x23")
        second.output.clear()
        second.send(b"\x1d2")
        assert second.expect(rb"PID=(\d+) TICK=\d+ SIZE=\d+x\d+")[1] == second_pid
        wait_for_child_size(second_log, "100x29")
        second.expect(re.escape(b"[2:Second]"))
        second.send(b"\x1dd")
        second.wait(0)

    verify_tab_move(command, directory, first_pid, second_pid)


def verify_tab_move(command, directory, moved_pid, visible_pid):
    attach = [*command, "attach"]
    marker = rb"PID=(\d+) TICK=\d+ SIZE=100x29"
    start = [
        *attach, "--new-workspace", "Destination", "--", sys.executable,
        __file__, "--child", directory / "destination-tab",
    ]
    with OuterTerminal(start) as destination:
        destination.expect(rb"PID=\d+ TICK=\d+ SIZE=80x23")
        destination.send(b"\x1dd")
        destination.wait(0)

    def inventory():
        return subprocess.check_output([*command, "control", "workspaces"], text=True, timeout=3)

    layout = inventory()
    source_id = re.search(r"^workspace ([\da-f-]+) Main$", layout, re.M)[1]
    destination_id = re.search(r"^workspace ([\da-f-]+) Destination$", layout, re.M)[1]
    moved_tab = re.search(r"^  tab ([\da-f-]+) Pane 1$", layout, re.M)[1]
    visible_tab = re.search(r"^  tab ([\da-f-]+) Second$", layout, re.M)[1]
    move = [
        *attach, "--workspace", source_id, "--tab", visible_tab,
        "--claim-workspace", destination_id, "--move-tab", moved_tab, "--to-workspace", destination_id,
    ]
    with OuterTerminal(move, cols=100, rows=30) as visible:
        assert visible.expect(marker)[1] == visible_pid
        deadline = time.monotonic() + 5
        while f"  tab {moved_tab} Pane 1" not in inventory().split(" Destination\n", 1)[1]:
            assert time.monotonic() < deadline, "tab did not move to the destination"
            time.sleep(0.02)
        visible.send(b"\x1dd")
        visible.wait(0)

    with OuterTerminal([*attach, "--workspace", destination_id, "--tab", moved_tab],
                       cols=100, rows=30) as moved:
        assert moved.expect(marker)[1] == moved_pid
        moved.expect(b"CORVID  Destination")
        moved.send(b"\x1dd")
        moved.wait(0)


def verify_tab_picker(command, directory):
    attach = [*command, "attach"]
    child_command = [sys.executable, __file__, "--child"]
    marker = rb"PID=(\d+) TICK=\d+ SIZE=\d+x\d+"
    with OuterTerminal([*attach, "--new-workspace", "Picker", "--", *child_command,
                        directory / "picker-1"]) as first:
        pids = {1: first.expect(marker)[1]}
        first.send(b"\x1dd")
        first.wait(0)

    layout = subprocess.check_output([*command, "control", "workspaces"], text=True, timeout=3)
    workspace = re.search(r"^workspace ([\da-f-]+) Picker$", layout, re.M)[1]
    attach.extend(["--workspace", workspace])
    tenth = "열번째"
    for number in range(2, 12):
        name = tenth if number == 10 else f"Task {number}"
        with OuterTerminal([*attach, "--new-tab", name, "--", *child_command,
                            directory / f"picker-{number}"]) as tab:
            pids[number] = tab.expect(marker)[1]
            tab.send(b"q" if number == 11 else b"\x1dd")
            tab.wait(7 if number == 11 else 0)

    with OuterTerminal(attach, rows=6) as outer:
        assert outer.expect(marker)[1] == pids[1]
        outer.expect(re.escape(b"[1:Pane 1]"))
        outer.output.clear()
        outer.send(b"\x1dw")
        outer.expect(b"Tabs 1/11")
        outer.send(b"\x1b[B" * 8)
        outer.expect(b"Tabs 9/11")
        outer.expect(f"10: {tenth}".encode())
        outer.send(b"q" + PASTE_START + b"q" + PASTE_END + b"\x1b")
        assert outer.expect(marker)[1] == pids[1]

        outer.output.clear()
        outer.send(b"\x1d9")
        assert outer.expect(marker)[1] == pids[9]
        outer.expect(re.escape(b"[9:Task 9]"))
        outer.output.clear()
        outer.send(b"\x1dw")
        outer.expect(b"Tabs 9/11")
        outer.output.clear()
        outer.resize(60, 6)
        outer.expect(b"Tabs 9/11")
        outer.send(b"\x1bOB\x1b[B")
        outer.expect(b"Tabs 11/11")
        outer.expect(re.escape(b"11 (closed): Task 11"))
        outer.send(b"\r\x1b[A")
        outer.expect(b"Tabs 10/11")
        outer.output.clear()
        outer.send(b"\r")
        assert outer.expect(marker)[1] == pids[10]
        outer.expect(re.escape(f"[10:{tenth}]".encode()))
        outer.output.clear()
        outer.send(b"\x1dw")
        outer.expect(b"Tabs 10/11")
        outer.send(tenth.encode())
        outer.expect(b"Tabs 1/1")
        outer.output.clear()
        outer.send(b"\r")
        assert outer.expect(marker)[1] == pids[10]
        outer.send(b"\x1dd")
        outer.wait(0)


def background_daemon_pid(runtime):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.connect(os.fspath(runtime / "control.sock"))
        credentials = connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
    pid, uid, _ = struct.unpack("3i", credentials)
    assert uid == os.getuid()
    return pid


def verify_upgrade(command, runtime, home, env, old_pid):
    def control(request):
        return subprocess.check_output([*command, "control", request], timeout=5)

    old_host, old_epoch = control("host").splitlines()
    old_layout = control("workspaces")
    assert b"Upgrade layout" in old_layout
    replacement_env = env | {"XDG_STATE_HOME": str(home / "other-state")}
    deadline = time.monotonic() + 5
    while True:
        result = subprocess.run(
            [*command, "upgrade"], env=replacement_env, capture_output=True, timeout=12,
        )
        if result.returncode == 0:
            break
        assert b"DaemonBusy" in result.stderr and time.monotonic() < deadline, result
        time.sleep(0.02)

    assert b"Upgraded the Corvid daemon" in result.stdout
    assert background_daemon_pid(runtime) != old_pid
    new_host, new_epoch = control("host").splitlines()
    assert new_host == old_host and new_epoch != old_epoch
    assert control("workspaces") == old_layout
    assert not (home / "other-state").exists()


def verify_default_start(binary, directory):
    runtime = directory / "auto"
    home = directory / "home"
    home.mkdir()
    programs = directory / "shell-bin"
    programs.mkdir()
    for name in ("infocmp", "stty"):
        (programs / name).symlink_to(shutil.which(name))
    shell = programs / "fixture-shell"
    # Bash used as /bin/sh clears inherited PS1 in the noninteractive wrapper.
    shell.write_text("#!/bin/sh\nPS1='CORVID-PROMPT> ' exec /bin/sh -i\n")
    shell.chmod(0o700)
    env = os.environ | {
        "HOME": str(home), "SHELL": str(shell), "PS1": "CORVID-PROMPT> ",
        "XDG_STATE_HOME": str(home), "PATH": str(programs),
    }
    env.pop("ENV", None)
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
        terminal.send(
            b"infocmp \"$TERM\" >/dev/null && "
            b"printf 'CORVID-SHELL=%s SIZE=%s\\n' \"$$\" \"$(stty size)\"\n"
        )
        return terminal.expect(rb"CORVID-SHELL=(\d+) SIZE=79 319")[1]

    try:
        with (
            OuterTerminal(command, cols=319, rows=80, env=env) as first,
            OuterTerminal(command, cols=319, rows=80, env=env) as second,
        ):
            sessions = [first, second]
            deadline = time.monotonic() + 5
            while all(terminal.process.poll() is None for terminal in sessions):
                assert time.monotonic() < deadline, "both clients retained input ownership"
                time.sleep(0.02)
            active, = [terminal for terminal in sessions if terminal.process.poll() is None]
            retired, = [terminal for terminal in sessions if terminal is not active]
            # A concurrent shell creation can still be reserved; an established
            # terminal is resumed and the previous client can exit successfully
            # before it renders the shell prompt.
            assert retired.process.returncode in (0, 1)
            if retired.process.returncode == 1:
                retired.expect(b"WorkspaceBusy")
            retired.wait(retired.process.returncode)
            pid = shell_pid(active)
            daemon = background_daemon_pid(runtime)
            assert os.getsid(daemon) == daemon
            active.output.clear()
            active.send(b"\x1d")
            active.expect(b"CORVID CMD")
            active.send(b"d")
            active.wait(0)

        upgraded = subprocess.run(
            [*command, "upgrade"], env=env, capture_output=True, timeout=5,
        )
        assert upgraded.returncode == 0, upgraded
        daemon = background_daemon_pid(runtime)

        with OuterTerminal(command, cols=319, rows=80, env=env) as second:
            assert shell_pid(second) == pid
            assert background_daemon_pid(runtime) == daemon
            second.send(b"exit\n")
            second.wait(0)

        named_tab = [*command, "attach", "--new-tab", "Upgrade layout"]
        with OuterTerminal(named_tab, cols=319, rows=80, env=env) as tab:
            shell_pid(tab)
            tab.send(b"exit\n")
            tab.wait(0)

        with socket.socket(socket.AF_UNIX) as attaching:
            attaching.settimeout(3)
            attaching.connect(os.fspath(runtime / "client.sock"))
            assert attaching.recv(1), "daemon did not accept the pending TUI"
            upgraded = subprocess.run(
                [*command, "upgrade"], env=env, capture_output=True, timeout=5,
            )
            assert upgraded.returncode == 0, upgraded
            assert background_daemon_pid(runtime) != daemon
            daemon = background_daemon_pid(runtime)

        verify_upgrade(command, runtime, home, env, daemon)
        # Both saved shells exited above. Reopening a closed pane is explicit.
        with OuterTerminal([*command, "attach", "--new"], cols=319, rows=80, env=env) as updated:
            shell_pid(updated)
            updated.send(b"exit\n")
            updated.wait(0)
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


def verify_shell_with_agents(binary, directory, providers):
    root = directory / "-".join(providers)
    root.mkdir()
    programs = root / "bin"
    programs.mkdir()
    agent_log = root / "agent-invocations"
    shell = programs / "fixture-shell"
    shell.write_text("#!/bin/sh\nPS1='CORVID-SHELL> ' exec /bin/sh -i\n")
    shell.chmod(0o700)
    for provider in providers:
        script = programs / provider
        script.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' \"$0\" >> \"$CORVID_AGENT_LOG\"\n"
            "if [ \"$1\" = --version ]; then echo 'fixture version'; exit; fi\n"
            f"printf 'AGENT-{provider}=%s\\n' \"$$\"\n"
            "read -r line\n"
        )
        script.chmod(0o700)

    command = [binary, "--runtime-dir", root / "runtime"]
    client_programs = root / "client-bin"
    client_programs.mkdir()
    daemon_env = os.environ | {
        "PATH": str(programs),
        "SHELL": str(shell), "PS1": "CORVID-SHELL> ", "CORVID_AGENT_LOG": str(agent_log),
    }
    daemon_env.pop("ENV", None)
    client_env = os.environ | {"PATH": str(client_programs)}
    daemon = subprocess.Popen(
        [*command, "daemon", "--state-dir", root / "state"], cwd=root, env=daemon_env,
    )
    try:
        wait_for_daemon(command, daemon)
        with OuterTerminal(command, env=client_env) as terminal:
            terminal.expect(b"CORVID-SHELL>")
            assert not agent_log.exists(), "startup ran a provider"
            assert not subprocess.check_output([*command, "control", "agents"], timeout=3)
            terminal.send(b"\x1dd")
            terminal.wait(0)

        for provider in providers:
            start = [*command, "attach", "--new-tab", provider, "--agent", provider]
            with OuterTerminal(start, env=client_env) as terminal:
                terminal.expect(rf"AGENT-{provider}=\d+".encode())
                terminal.send(b"\x1dd")
                terminal.wait(0)

        agents = subprocess.check_output([*command, "control", "agents"], timeout=3)
        for provider in providers:
            assert f"provider={provider.title()} process=Running".encode() in agents, agents
    finally:
        stop(daemon)


def verify_resume(binary, directory):
    root = directory / "resume"
    root.mkdir()
    runtime = root / "run"
    views = root / "views"
    env = os.environ | {"CORVID_VIEW_STATE_DIR": str(views)}
    command = [binary, "--runtime-dir", runtime]
    daemon = subprocess.Popen([*command, "daemon", "--state-dir", root / "state"])
    try:
        wait_for_daemon(command, daemon)
        with OuterTerminal(
            [*command, "attach", "--new", "--", "/bin/sh", "-c", "echo RESUME_READY; exec /bin/sh"],
            env=env,
        ) as first:
            first.expect(b"RESUME_READY")
            first.send(b'kept=preserved; printf "FIRST_%s\\n" "$kept"\n')
            first.expect(b"FIRST_preserved")

            # The remembered selection is saved by a debounced background writer.
            saved = views / "tui" / "view.json"
            deadline = time.monotonic() + 5
            while not saved.exists():
                assert first.process.poll() is None, "original writer exited before saving its view"
                assert time.monotonic() < deadline, "original writer did not save its view"
                time.sleep(0.025)
            assert json.loads(saved.read_text())["views"][0]["target"]["runtime"] == str(runtime)

            # The normal attach command requires neither Observe nor a takeover flag.
            with OuterTerminal([*command, "attach"], cols=60, rows=20, env=env) as second:
                second.expect(b"FIRST_preserved")
                first.wait(0)
                second.send(b'printf "RESUMED_%s\\n" "$kept"; stty size\n')
                second.expect(rb"RESUMED_preserved[\s\S]*19 60")
                second.send(b"exit\n")
                second.wait(0)
    finally:
        stop(daemon)


def verify_shell_agent_notifications(
    binary, directory, ssh=None, shell_name="sh", *, release="baseline", broker=False,
):
    root = directory / f"notify-{shell_name}-{release}{'-broker' if broker else ''}"
    root.mkdir()
    programs = root / "bin"
    programs.mkdir()
    home = root / "home"
    home.mkdir()
    if shell_name == "sh":
        shell = root / "shell"
        shell.write_text("#!/bin/sh\nPS1='NOTIFY-SHELL> ' exec /bin/sh -i\n")
        shell.chmod(0o700)
    else:
        shell = shutil.which(shell_name)
        if shell_name == "zsh":
            # Keep the real zsh integration and this fixture's .zshrc, without
            # runner-wide completion setup prompting about unrelated directories.
            (home / ".zshenv").write_text("unsetopt GLOBAL_RCS\n")
        profile = ".bash_profile" if shell_name == "bash" else ".zshrc"
        (home / profile).write_text(
            f'export PATH={shlex.quote(str(programs))}:"$PATH"\n'
            "PS1='NOTIFY-SHELL> '\n"
        )
    fixture = Path(__file__).with_name("fixtures") / "shell_agent.rs"
    versions = fixture.parents[3] / "corvid-protocol/src/collector.rs"
    build_env = os.environ.copy()
    for name in ("CLAUDE", "CODEX"):
        build_env[f"CORVID_FIXTURE_{name}"] = re.search(
            rf'{name}_VERSION: &str = "([^"]+)"', versions.read_text(),
        )[1]
    for name, version in PROVIDER_RELEASES[release].items():
        build_env[f"CORVID_FIXTURE_{name}"] = version
    subprocess.run(["rustc", "--edition=2024", fixture, "-o", programs / "claude"], env=build_env, check=True)
    (programs / "codex").hardlink_to(programs / "claude")

    env = os.environ | {
        "SHELL": str(shell), "HOME": str(home), "PS1": "NOTIFY-SHELL> ",
        "PATH": str(programs), "CORVID_FIXTURE_BINARY": str(Path(binary).resolve()),
        "CLAUDE_CONFIG_DIR": str(root / "claude-config"), "CODEX_HOME": str(root / "codex-home"),
    }
    env.pop("CORVID_FIXTURE_BROKER", None)
    if broker:
        target = f"{os.uname().machine}-unknown-linux-musl"
        version = build_env["CORVID_FIXTURE_CODEX"]
        executable = root / "packages/app-server-daemon/releases" / f"{version}-{target}/bin/codex"
        executable.parent.mkdir(parents=True)
        shutil.copy2(programs / "codex", executable)
        env["CORVID_FIXTURE_BROKER"] = str(executable)
    for key in ("ENV", "BASH_ENV", "ZDOTDIR"):
        env.pop(key, None)
    client_env = os.environ.copy()
    client_env.pop("CORVID_NOTIFY", None)
    command = [binary, "--runtime-dir", root / "run"]
    if ssh is None:
        client_env["SSH_TTY"] = "/dev/pts/fixture"
        attach = command
    else:
        attach = [*ssh, "-tt", shlex.join(map(os.fspath, ["env", "CORVID_AGENT_MONITOR=0", *command]))]
    daemon = subprocess.Popen([*command, "daemon", "--state-dir", root / "state"], env=env)
    try:
        wait_for_daemon(command, daemon)
        with OuterTerminal(attach, env=client_env) as terminal:
            terminal.expect(b"NOTIFY-SHELL>")
            assert not subprocess.check_output([*command, "control", "agents"], timeout=3)
            terminal.send(b"claude --version\n")
            terminal.expect(build_env["CORVID_FIXTURE_CLAUDE"].encode() + rb" \(Claude Code\)")

            # A nested process cannot register its own run in the shell's PTY.
            for provider in ("claude", "codex"):
                terminal.send(f"/bin/sh -c '{provider}; :'\n".encode())
                terminal.expect(f"NO-OBSERVERS-{provider}".encode())
                assert not subprocess.check_output([*command, "control", "agents"], timeout=3)
            for provider in ("claude", "codex"):
                terminal.output.clear()
                terminal.send(f"{provider}\n".encode())
                terminal.expect(f"READY-{provider}".encode())
                agents = subprocess.check_output([*command, "control", "agents"], timeout=3)
                assert f"provider={provider.title()} process=Running".encode() in agents, agents
                terminal.send(b"respond\n")
                terminal.expect(b"\x07")
                terminal.expect(f"DONE-{provider}".encode())
                terminal.expect(rb"DONE-" + provider.encode() + rb"[\s\S]*NOTIFY-SHELL>")
                assert terminal.output.count(b"\x07") == 1, terminal.output
                # Deck consumes this same daemon attention through the companion.
                run_ids = [
                    line.split()[0] for line in agents.splitlines()
                    if b"provider=" + provider.title().encode() + b" " in line
                ]
                assert len(run_ids) == 1, agents
                attention = subprocess.check_output([*command, "control", "attention"], timeout=3)
                responses = [
                    line for line in attention.splitlines()
                    if b"run_id=" + run_ids[0] + b" " in line and b"reason=Response " in line
                ]
                assert responses, attention
            terminal.send(b"exit\n")
            terminal.wait(0)
    finally:
        stop(daemon)


def verify_workspace_ui(command, directory):
    env = os.environ | {"CORVID_LEGACY_TUI": "0"}
    mouse_marker = directory / "app-mouse-input"

    def send_line(terminal, text):
        # Separate the terminal's input echo from output and wait for the side effect.
        terminal.send(text.encode() + b"\r")

    def show_marker(terminal, label, suffix, *, setup=""):
        # Keep the marker visible after changing the PTY size. CI hostnames in
        # the default prompt can wrap across several rows of a split pane.
        terminal.output.clear()
        send_line(
            terminal,
            f"PS1='$ '; {setup}printf '\\033[2J\\033[H{label}-%s\\n' {suffix}",
        )
        terminal.expect(f"{label}-{suffix}".encode())

    def workspace_panes():
        layout = subprocess.check_output([*command, "control", "workspaces"], text=True, timeout=3)
        workspace = layout.split(" Workspace UI\n", 1)[1].split("\nworkspace ", 1)[0]
        return re.findall(r"^    pane ([\da-f-]+)$", workspace, re.M)

    def wait_for_writer_mode(terminal, mode):
        deadline = time.monotonic() + 5
        while True:
            clients = subprocess.check_output([*command, "control", "clients"], text=True, timeout=3)
            writers = []
            for client in clients.splitlines():
                identity, *fields = client.split()
                view = dict(field.split("=", 1) for field in fields)
                if "pane-writer" in view:
                    writers.append((identity, view))
            if len(writers) == 1 and writers[0][1].get("mode") == mode:
                return writers[0]
            assert terminal.process.poll() is None, ("TUI exited", terminal.output[-500:], clients)
            assert time.monotonic() < deadline, (mode, clients)
            # An unread redraw can fill the outer PTY before the client reads
            # another key. Keep the bytes for the following redraw assertion.
            terminal.read_output(0.02)
            assert len(terminal.output) <= 1024 * 1024, "unexpected output volume"

    pane_hint = rb"\x1b\[30;1H\x1b\[0;7m\x1b\[2KPane "
    cursor_visible = rb"\x1b\[\?25h"
    launch = [*command, "attach", "--new-workspace", "Workspace UI"]
    with OuterTerminal(launch, cols=100, rows=30, env=env) as terminal:
        terminal.expect(rb"Ctrl\+\] commands")
        terminal.send(b"\x1d?")
        terminal.expect(b"CORVID COMMANDS")
        terminal.output.clear()
        terminal.send(b"\x1b")
        time.sleep(0.15)
        terminal.expect(rb"Ctrl\+\] commands")
        show_marker(terminal, "left", "PANE", setup="export CORVID_TEST_PANE=left; ")

        terminal.send(b"\x1dpr")
        terminal.expect(b"Pane 2")
        show_marker(terminal, "top", "PANE", setup="export CORVID_TEST_PANE=top; ")
        terminal.send(b"\x1dpd")
        terminal.expect(b"Pane 3")
        show_marker(terminal, "bottom", "PANE", setup="export CORVID_TEST_PANE=bottom; ")

        # Mouse coordinates are relative to the outer screen. This is inside the left pane.
        terminal.output.clear()
        terminal.send(b"\x1b[<0;4;5M")
        terminal.expect(rb"\x1b\[2;1H\x1b\[0;1;36m" + "● ".encode())
        send_line(terminal, 'printf "FOCUSED-%s\\n" "$CORVID_TEST_PANE"')
        terminal.expect(b"FOCUSED-left")

        terminal.output.clear()
        terminal.send(b"\x1d\x1b[C")
        terminal.expect(rb"\x1b\[2;51H\x1b\[0;1;36m" + "● ".encode())
        send_line(terminal, shlex.join((sys.executable, __file__, "--mouse-child", str(mouse_marker))))
        terminal.expect(b"EDITOR-READY")
        terminal.output.clear()
        terminal.send(b"\x1b[<0;5;6M")
        terminal.expect(rb"\x1b\[2;1H\x1b\[0;1;36m" + "● ".encode())
        terminal.send(b"\x1b[<0;55;6M")
        terminal.expect(b"APP-MOUSE-RECEIVED")
        assert mouse_marker.read_bytes() == b"\x1b[<0;5;4M\x1b[<0;5;4m"
        show_marker(terminal, "top", "PREVIEW")
        terminal.send(b"\x1dd")
        terminal.wait(0)

    with OuterTerminal([*command, "attach"], cols=100, rows=30, env=env) as terminal:
        terminal.expect(b"left-PANE")
        terminal.expect(b"top-PREVIEW")
        terminal.expect(b"bottom-PANE")
        terminal.send(b"\x1d.")
        terminal.expect(b"Rename tab")
        terminal.send(b"work\r")
        terminal.expect(rb"\[1 work\]")
        terminal.send(b"\x1dt")
        terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m\[1 work\]")
        terminal.expect(b"n New")
        terminal.send(b"n")
        terminal.expect(rb"\x1b\[0;1;36m\[2 ")
        show_marker(terminal, "INPUT", "NEW_TAB")
        terminal.send(b"\x1d.")
        terminal.expect(b"Rename tab")
        terminal.send(b"hello\r")
        terminal.expect(rb"\[2 hello\]")
        split_markers = (b"left-PANE", b"top-PREVIEW", b"bottom-PANE")
        cases = (
            (b"h", b"work", b"1", split_markers, b"INPUT-NEW_TAB"),
            (b"\x1bOC", b"hello", b"2", (b"INPUT-NEW_TAB",), b"left-PANE"),
            (b"\x1b[D", b"work", b"1", split_markers, b"INPUT-NEW_TAB"),
            (b"l", b"hello", b"2", (b"INPUT-NEW_TAB",), b"left-PANE"),
        )
        for index, (key, label, tab, markers, previous) in enumerate(cases):
            active = rb"\x1b\[0;1;36m\[" + tab + b" " + label + rb"\]"
            terminal.output.clear()
            terminal.send(b"\x1dt" + key)
            terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m" + tab + b" " + label)
            for marker in markers:
                match = terminal.expect(marker)
                frame = terminal.output[terminal.output.rfind(b"\x1b[2J", 0, match.start()):]
                assert previous not in frame, frame
                assert b"\x1b[?25h" not in frame, frame
            assert not re.search(active, terminal.output), terminal.output
            terminal.output.clear()
            terminal.send(b"q\r")
            terminal.expect(active)
            terminal.expect(cursor_visible)
            send_line(terminal, f"printf 'WRITABLE-%s\\n' {index}")
            terminal.expect(f"WRITABLE-{index}".encode())
        terminal.output.clear()
        terminal.send(b"\x1d[")
        terminal.expect(rb"\x1b\[2;51H\x1b\[0;1;36m" + "● ".encode())
        current_tab = terminal.expect(rb"\x1b\[1;(\d+)H\x1b\[0;1;36m\[1 ").group(1)
        terminal.send(b"\x1d]")
        terminal.expect(rb"\[2 ")
        terminal.output.clear()
        terminal.send(b"\x1b[<0;" + current_tab + b";1M")
        terminal.expect(rb"\x1b\[2;51H\x1b\[0;1;36m" + "● ".encode())
        terminal.output.clear()
        terminal.send(b"\x1dtl")
        terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m2 hello")
        terminal.expect(b"INPUT-NEW_TAB")
        terminal.send(b"\x1b")
        terminal.output.clear()
        terminal.expect(rb"\x1b\[0;1;36m\[1 work\][^\r\n]*Ctrl\+\] commands")
        send_line(terminal, 'printf "STILL-%s\\n" "$CORVID_TEST_PANE"')
        terminal.expect(b"STILL-top")

        terminal.output.clear()
        terminal.send(b"\x1dtl[")
        terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m1 hello")
        terminal.expect(b"INPUT-NEW_TAB")
        terminal.output.clear()
        terminal.send(b"]")
        terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m2 hello")
        terminal.output.clear()
        terminal.send(b"\x1b")
        terminal.expect(rb"\x1b\[0;1;36m\[1 work\][^\r\n]*Ctrl\+\] commands")

        terminal.output.clear()
        terminal.send(b"\x1dtlx")
        terminal.expect(rb"\x1b\[0;1;36m\[1 work\][^\r\n]*Ctrl\+\] commands")
        send_line(terminal, 'printf "AFTER-CLOSE-%s\\n" "$CORVID_TEST_PANE"')
        terminal.expect(b"AFTER-CLOSE-top")
        terminal.output.clear()
        terminal.send(b"\x1dtl")
        terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m\[1 work\]")
        terminal.expect(b"x Close")
        assert b"2 hello" not in terminal.output, terminal.output
        terminal.send(b"\x1b")
        terminal.output.clear()
        terminal.expect(rb"Ctrl\+\] commands")

        terminal.output.clear()
        terminal.send(b"\x1dp")
        terminal.expect(pane_hint + b"2")
        for key, number in (
            (b"j", b"3"), (b"k", b"2"), (b"h", b"1"), (b"l", b"2"), (b"\x1b[B", b"3"),
        ):
            terminal.output.clear()
            terminal.send(key)
            terminal.expect(pane_hint + number)
        terminal.output.clear()
        terminal.send(b"\x1b")
        terminal.expect(cursor_visible)
        send_line(terminal, 'printf "CANCELLED-%s\\n" "$CORVID_TEST_PANE"')
        terminal.expect(b"CANCELLED-top")

        terminal.output.clear()
        terminal.send(b"\x1dpj")
        terminal.expect(pane_hint + b"3")
        terminal.output.clear()
        terminal.send(b"\r")
        terminal.expect(cursor_visible)
        send_line(terminal, 'printf "SELECTED-%s\\n" "$CORVID_TEST_PANE"')
        terminal.expect(b"SELECTED-bottom")
        terminal.output.clear()
        terminal.send(b"\x1d\x1b[A")
        terminal.expect(rb"\x1b\[2;51H\x1b\[0;1;36m" + "● ".encode())

        # Resizing can reflow retained output, so check it after the reattach assertions.
        terminal.output.clear()
        terminal.resize(20, 6)
        terminal.expect(rb"\x1b\[1;1H\x1b\[0m\x1b\[2K\x1b\[0;2mWorksp\x1b")
        terminal.send(b"\x1dpj")
        terminal.expect(rb"\x1b\[6;1H\x1b\[0;7m\x1b\[2KPane 3")
        terminal.output.clear()
        terminal.send(b"\x1b")
        client, narrow_view = wait_for_writer_mode(terminal, "Passthrough")
        terminal.expect(cursor_visible)
        terminal.output.clear()
        terminal.resize(100, 30)
        restored_split = (
            rb"\x1b\[2J\x1b\[1;1H\x1b\[0m(?s:.*?)"
            rb"\x1b\[2;51H\x1b\[0;1;36m" + "● ".encode()
            + rb"(?s:.*?)" + cursor_visible
        )
        # The narrow viewport cannot draw the right-hand pane at column 51.
        # A complete restored frame follows the resize's new input route.
        terminal.expect(restored_split)
        restored_client, restored_view = wait_for_writer_mode(terminal, "Passthrough")
        assert restored_client == client
        assert restored_view["pane-writer"] == narrow_view["pane-writer"]
        assert restored_view["terminal"] == narrow_view["terminal"]
        assert int(restored_view["view_revision"]) > int(narrow_view["view_revision"])

        terminal.output.clear()
        terminal.send(b"\x1dw")
        terminal.expect(b"Workspaces")
        terminal.output.clear()
        terminal.send(b"\x1dd")
        terminal.expect(b"Search: d")
        terminal.output.clear()
        terminal.send(b"\x1b")
        wait_for_writer_mode(terminal, "Passthrough")
        terminal.expect(restored_split)
        send_line(terminal, 'printf "AFTER-RESIZE-%s\\n" "$CORVID_TEST_PANE"')
        terminal.expect(b"AFTER-RESIZE-top")

        # Closing the highlighted lower pane leaves input on the upper pane.
        left_pane, top_pane, _ = workspace_panes()
        terminal.output.clear()
        terminal.send(b"\x1dpjx")
        deadline = time.monotonic() + 5
        while (remaining := workspace_panes()) != [left_pane, top_pane]:
            assert time.monotonic() < deadline, remaining
            time.sleep(0.02)
        terminal.expect(cursor_visible)
        send_line(terminal, 'printf "PANE-CLOSED-%s\\n" "$CORVID_TEST_PANE"')
        terminal.expect(b"PANE-CLOSED-top")
        terminal.send(b"\x1dtx")
        terminal.wait(0)


def verify_overflowing_titles(command):
    env = os.environ | {"CORVID_LEGACY_TUI": "0"}
    with OuterTerminal(
        [*command, "attach", "--new-workspace", "workspace-review-WEST"],
        cols=40, rows=12, env=env,
    ) as terminal:
        terminal.expect(rb"workspace-rev")
        terminal.send(b"\x1dtn")
        terminal.expect(rb"\x1b\[0;1;36m\[2 ")
        terminal.output.clear()
        terminal.send(b"\x1d[")
        terminal.expect(rb"\x1b\[0;1;36m\[1 ")
        terminal.send(b"\x1d.")
        terminal.expect(rb"Rename tab")
        terminal.send("shared-prefix-한글-TAIL\r".encode())
        active_tab = terminal.expect(rb"\x1b\[1;(\d+)H\x1b\[0;1;36m\[1 shared-prefix").group(1)
        terminal.output.clear()

        # While the shell is idle, the next animation frame changes only the header.
        frame = terminal.expect(rb"\x1b7(?:(?!\x1b8).)*TAIL\].*?\x1b8").group()
        assert b"\x1b[2J" not in frame
        assert b"\x1b[1;" + active_tab + b"H" in frame
        second_tab = re.search(rb"\x1b\[1;(\d+)H\x1b\[0m2 ", frame)[1]
        terminal.expect(rb"\x1b7(?:(?!\x1b8).)*WEST(?:(?!\x1b8).)*\x1b8")
        terminal.output.clear()
        terminal.send(b"\x1b[<0;" + second_tab + b";1M")
        terminal.expect(rb"\x1b\[0;1;36m\[2 ")
        full_redraw = rb"\x1b\[2J\x1b\[1;1H\x1b\[0m"
        header_redraw = full_redraw + rb"\x1b\[2K"
        shell_redraw = (
            full_redraw + rb"\x1b\[0(?:;\d+)*m"
            rb"(?:(?!\x1b\[\?25).)*\x1b\[\?25h"
        )
        terminal.output.clear()
        terminal.resize(20, 12)
        terminal.expect(header_redraw + rb"\x1b\[0;2mworksp\x1b")
        terminal.output.clear()
        terminal.resize(20, 2)
        # At two rows the normal redraw starts with the shell and ends with a visible cursor.
        # A row-two cursor move alone can belong to the old viewport.
        terminal.expect(shell_redraw)
        terminal.output.clear()
        terminal.send(b"\x1dt")
        terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m\[2 ")
        terminal.output.clear()
        terminal.send(b"h")
        terminal.expect(rb"\x1b\[1;\d+H\x1b\[0;1;7m1 shared-")
        terminal.output.clear()
        terminal.send(b"\x1b")
        terminal.expect(shell_redraw)
        terminal.output.clear()
        terminal.resize(20, 12)
        terminal.expect(header_redraw + rb"\x1b\[0;2m[^\x1b]*\x1b\[1;\d+H\x1b\[0;1;36m\[2 ")
        terminal.send(b"\x1dw")
        terminal.expect(rb"Workspaces")
        terminal.expect(rb"\x1b\[\d+;1H\x1b\[0m\x1b\[2K> [^\x1b]*WEST")
        terminal.send(b"\x1b")
        time.sleep(0.15)
        terminal.send(b"\x1dd")
        terminal.wait(0)


def run(binary, *, tty_only=False):
    binary = str(Path(binary).resolve())
    os.environ["TERM"] = "xterm-256color"
    # The runtime directory must have private, trusted parents.
    with tempfile.TemporaryDirectory(prefix="corvid-test-", dir=Path.home()) as temporary:
        directory = Path(temporary)
        os.environ["CORVID_VIEW_STATE_DIR"] = str(directory / "views")
        command = [binary, "--runtime-dir", directory / "runtime"]
        daemon = subprocess.Popen(
            [*command, "daemon", "--state-dir", directory / "state"],
            env=os.environ,
        )
        try:
            wait_for_daemon(command, daemon)
            refused = subprocess.run([*command, "upgrade"], capture_output=True, timeout=5)
            assert refused.returncode == 1 and b"UnsupportedCapability" in refused.stderr
            assert daemon.poll() is None
            verify(command, directory)
            verify_paste(command)
            verify_tabs(command, directory)
            verify_tab_picker(command, directory)
            verify_workspace_ui(command, directory)
            verify_overflowing_titles(command)
        finally:
            stop(daemon)
        verify_resume(binary, directory)
        print("PASS live TTY input, paste, resize, redraw, pairing, menu isolation, tabs, detach, resume, and TTY restoration.", flush=True)
        if tty_only:
            return
        verify_default_start(binary, directory)
        for providers in [("claude",), ("codex",), ("claude", "codex")]:
            verify_shell_with_agents(binary, directory, providers)
        verify_shell_agent_notifications(binary, directory)
        for release in ("older", "newer"):
            for broker in (False, True):
                verify_shell_agent_notifications(
                    binary, directory, release=release, broker=broker,
                )
        for shell in ("bash", "zsh"):
            if shutil.which(shell):
                verify_shell_agent_notifications(
                    binary, directory, shell_name=shell, release="newer",
                )
    print("Live TTYs verified shell-first startup, agent notifications, resize, redraw, input, command-menu isolation, detach, tab switching, tab search, and daemon replacement with a retained shell.")


if __name__ == "__main__":
    if sys.argv[1:2] == ["--mouse-child"]:
        sys.exit(mouse_child(Path(sys.argv[2])))
    if sys.argv[1:2] == ["--child"]:
        sys.exit(child(Path(sys.argv[2])))
    if sys.argv[1:2] == ["--paste-child"]:
        sys.exit(paste_child())
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--tty-only", action="store_true",
                        help="run the portable Linux/macOS TTY suite without Linux process and provider checks")
    args = parser.parse_args()
    if not args.tty_only and sys.platform != "linux":
        parser.error("the full suite requires Linux; use --tty-only on macOS")
    run(args.binary, tty_only=args.tty_only)
