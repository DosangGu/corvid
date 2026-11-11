"""Exercise a live daemon through separate Linux client TTYs."""

import fcntl
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
            assert select.select([self.master], [], [], wait)[0], (
                self.output[:700], self.output[-500:]
            )
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


def verify_resize(outer, progress, cols, rows, inner_size):
    outer.resize(cols, rows)
    footer = f"\x1b[{rows};1H\x1b[0;30;46m\x1b[2KCORVID".encode()
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
        first.expect(re.escape(b"\x1b[24;1H\x1b[0;30;46m\x1b[2KCORVID  Main"))
        pid, tick = int(initial[1]), int(initial[2])
        first.send(b"a")
        first.expect("한글 e\u0301".encode())
        verify_pairing(command, first, pid)

        with OuterTerminal([*command, "attach"], cols=110, rows=31) as busy:
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
        second.expect(re.escape(b"[1:Shell]"))
        second.resize(100, 30)
        second.expect(b"RESIZED=100x29")
        deadline = time.monotonic() + 5
        while not second_log.read_text().strip().endswith("100x29"):
            assert time.monotonic() < deadline, "hidden tab kept the old PTY size"
            time.sleep(0.02)
        second.output.clear()
        second.send(b"\x1d2")
        assert second.expect(rb"PID=(\d+) TICK=\d+ SIZE=100x29")[1] == second_pid
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
        outer.expect(re.escape(b"[1:Shell]"))
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
            b"infocmp corvid >/dev/null && "
            b"printf 'CORVID-SHELL=%s SIZE=%s\\n' \"$$\" \"$(stty size)\"\n"
        )
        return terminal.expect(rb"CORVID-SHELL=(\d+) SIZE=79 319")[1]

    try:
        with (
            OuterTerminal(command, cols=319, rows=80, env=env) as first,
            OuterTerminal(command, cols=319, rows=80, env=env) as second,
        ):
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
        with OuterTerminal(command, cols=319, rows=80, env=env) as updated:
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


def verify_shell_with_agents(binary, directory, terminfo, providers):
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
        "PATH": str(programs), "TERMINFO": str(terminfo),
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


def verify_shell_agent_notifications(binary, directory, terminfo, ssh=None, shell_name="sh"):
    root = directory / f"notify-{shell_name}"
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
    subprocess.run(["rustc", fixture, "-o", programs / "claude"], env=build_env, check=True)
    (programs / "codex").hardlink_to(programs / "claude")

    env = os.environ | {
        "TERMINFO": str(terminfo), "SHELL": str(shell), "HOME": str(home), "PS1": "NOTIFY-SHELL> ",
        "PATH": str(programs), "CORVID_FIXTURE_BINARY": str(Path(binary).resolve()),
        "CLAUDE_CONFIG_DIR": str(root / "claude-config"), "CODEX_HOME": str(root / "codex-home"),
    }
    for key in ("ENV", "BASH_ENV", "ZDOTDIR"):
        env.pop(key, None)
    client_env = os.environ.copy()
    client_env.pop("CORVID_NOTIFY", None)
    command = [binary, "--runtime-dir", root / "run"]
    if ssh is None:
        client_env["SSH_TTY"] = "/dev/pts/fixture"
        attach = command
    else:
        attach = [*ssh, "-tt", shlex.join(map(os.fspath, command))]
    daemon = subprocess.Popen([*command, "daemon", "--state-dir", root / "state"], env=env)
    try:
        wait_for_daemon(command, daemon)
        with OuterTerminal(attach, env=client_env) as terminal:
            terminal.expect(b"NOTIFY-SHELL>")
            assert not subprocess.check_output([*command, "control", "agents"], timeout=3)
            terminal.send(b"claude --version\n")
            terminal.expect(build_env["CORVID_FIXTURE_CLAUDE"].encode() + rb" \(Claude Code\)")

            # A nested process cannot register its own run in the shell's PTY.
            terminal.send(b"/bin/sh -c 'claude; :'\n")
            terminal.expect(b"NO-OBSERVERS-claude")
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
            terminal.send(b"exit\n")
            terminal.wait(0)
    finally:
        stop(daemon)


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
            refused = subprocess.run([*command, "upgrade"], capture_output=True, timeout=5)
            assert refused.returncode == 1 and b"UnsupportedCapability" in refused.stderr
            assert daemon.poll() is None
            verify(command, directory)
            verify_paste(command)
            verify_tabs(command, directory)
            verify_tab_picker(command, directory)
        finally:
            stop(daemon)
        verify_default_start(binary, directory)
        for providers in [("claude",), ("codex",), ("claude", "codex")]:
            verify_shell_with_agents(binary, directory, database, providers)
        verify_shell_agent_notifications(binary, directory, database)
        for shell in ("bash", "zsh"):
            if shutil.which(shell):
                verify_shell_agent_notifications(binary, directory, database, shell_name=shell)
    print("Live TTYs verified shell-first startup, agent notifications, resize, redraw, input, detach, tab switching, the tab picker, and daemon replacement with a retained shell.")


if __name__ == "__main__":
    if sys.argv[1] == "--child":
        sys.exit(child(Path(sys.argv[2])))
    if sys.argv[1] == "--paste-child":
        sys.exit(paste_child())
    run(sys.argv[1])
