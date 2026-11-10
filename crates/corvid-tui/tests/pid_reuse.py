"""Force an actual PID reuse inside a disposable Linux PID namespace."""

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from companion_fixture import Companion, assigned_slots, fields
from interactive import OuterTerminal, stop, wait_for_daemon


PID = 4096
PID_CURSOR = Path("/proc/sys/kernel/ns_last_pid")
PAIRING_CODE = rb"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"
LAUNCHER = r'''
import os
from pathlib import Path
import sys
import tty

if sys.argv[1] == "--version":
    # The version probe runs just before the PTY child is spawned.
    cursor = Path(os.environ["CORVID_TEST_PID_CURSOR"])
    Path("/proc/sys/kernel/ns_last_pid").write_text(cursor.read_text())
    raise SystemExit(1)

label = sys.argv[1]
trace = Path(__file__).with_name(label + ".input")
tty.setraw(0)
os.write(1, f"PID={os.getpid()} {label}-READY".encode())
while data := os.read(0, 4096):
    with trace.open("ab") as stream:
        stream.write(data)
    if data == b"q":
        raise SystemExit(0)
'''


def control(command, request):
    return subprocess.check_output([*command, "control", request], text=True, timeout=3)


def wait_for_run(command, predicate):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        for line in control(command, "agents").splitlines():
            run = dict(fields(line), run=line.split()[0])
            if predicate(run):
                return run
        time.sleep(0.02)
    raise AssertionError("managed run did not reach the expected state")


def process_image(pid):
    process = Path(f"/proc/{pid}")
    start = int((process / "stat").read_text().rsplit(") ", 1)[1].split()[19])
    image = (process / "exe").stat()
    return start, (image.st_dev, image.st_ino)


def pair(companion, terminal, ready):
    terminal.output.clear()
    terminal.send(b"\x1dr")
    terminal.expect(b"Controller pairing code")
    code = terminal.expect(PAIRING_CODE)[0].decode()
    reply = fields(companion.run("pair", input=code + "\n").stdout)
    terminal.output.clear()
    terminal.send(b"\r")
    terminal.expect(ready)
    return reply["client_id"]


def slot_and_binding(companion, run, client):
    view = companion.wait(
        lambda view: run in assigned_slots(view)
        and any(binding["client_id"] == client for binding in view["bindings"])
    )
    binding, = [binding for binding in view["bindings"] if binding["client_id"] == client]
    return assigned_slots(view)[run], binding, view["header"]


def focus_slot(companion, slot, binding, *, check=True):
    return companion.run(
        "focus-slot", "--slot", slot["capture"], "--binding", binding["capture"], check=check
    )


def launch_and_retire_original(command, companion, attach):
    with OuterTerminal([*attach, "--", "FIRST"]) as terminal:
        ready = f"PID={PID} FIRST-READY".encode()
        terminal.expect(ready)
        old = wait_for_run(
            command, lambda run: run.get("pid") == str(PID) and run["process"] == "Running"
        )
        start, executable = process_image(PID)
        client = pair(companion, terminal, ready)
        slot, binding, _ = slot_and_binding(companion, old["run"], client)
        result = fields(focus_slot(companion, slot, binding).stdout)
        assert result["terminal_id"] == old["terminal"]

        terminal.send(b"q")
        terminal.wait(0)
    wait_for_run(command, lambda run: run["run"] == old["run"] and run["process"] == "Exited")
    assert not Path(f"/proc/{PID}").exists(), "original process was not reaped"
    return old, start, executable, slot


def reject_old_focus(command, companion, old, old_slot, binding, header):
    original_view = control(command, "clients")
    original_layout = control(command, "workspaces")
    result = subprocess.run(
        [
            *command, "controller",
            "--controller-id", companion.controller,
            "--host-id", companion.host,
            "--server-epoch", header["remote_epoch"],
            "focus",
            "--client-id", binding["client_id"],
            "--binding-revision", binding["revision"],
            "--run-id", old["run"],
        ],
        capture_output=True,
        text=True,
        timeout=3,
    )
    assert result.returncode != 0 and "TargetGone" in result.stderr, result.stderr
    stale = focus_slot(companion, old_slot, binding, check=False)
    assert stale.returncode != 0 and "StaleBinding" in stale.stderr, stale.stderr
    assert control(command, "clients") == original_view
    assert control(command, "workspaces") == original_layout


def switch_to_shell(command, terminal, previous_terminal):
    terminal.send(b"\x1dt")
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        current = fields(control(command, "clients")).get("terminal")
        if current and current != previous_terminal:
            return
        time.sleep(0.02)
    raise AssertionError("TUI did not switch to its shell tab")


def verify(command, companion, directory):
    layout = control(command, "workspaces")
    workspace, tab, _ = [line.split()[1] for line in layout.splitlines()]
    attach = [*command, "attach", "--workspace", workspace, "--tab", tab, "--agent", "claude"]
    old, old_start, old_executable, old_slot = launch_and_retire_original(
        command, companion, attach
    )

    with OuterTerminal([*attach, "--", "SECOND"]) as terminal:
        ready = f"PID={PID} SECOND-READY".encode()
        terminal.expect(ready)
        replacement = wait_for_run(
            command,
            lambda run: run.get("pid") == str(PID)
            and run["run"] != old["run"]
            and run["process"] == "Running",
        )
        start, executable = process_image(PID)
        assert start > old_start and executable == old_executable
        assert replacement["terminal"] != old["terminal"]
        assert control(command, "workspaces") == layout
        client = pair(companion, terminal, ready)
        slot, binding, header = slot_and_binding(companion, replacement["run"], client)
        assert slot["slot"] == old_slot["slot"] and int(slot["revision"]) > int(old_slot["revision"])

        switch_to_shell(command, terminal, replacement["terminal"])
        reject_old_focus(command, companion, old, old_slot, binding, header)

        terminal.output.clear()
        result = fields(focus_slot(companion, slot, binding).stdout)
        assert result["terminal_id"] == replacement["terminal"]
        terminal.expect(ready)
        assert not (directory / "SECOND.input").exists(), "focus injected input into the replacement"
        terminal.send(b"q")
        terminal.wait(0)


def run(binary, terminfo_source):
    uid_map = Path("/proc/self/uid_map").read_text().split()
    if (
        os.getpid() != 1
        or Path("/proc/self/stat").read_text().split()[0] != "1"
        or len(uid_map) != 3
        or uid_map[0] != "0"
        or uid_map[2] != "1"
    ):
        raise RuntimeError("run as PID 1 with fresh procfs and a single-UID user namespace")

    with tempfile.TemporaryDirectory(prefix="corvid-pid-", dir="/run") as temporary:
        directory = Path(temporary)
        cursor = directory / "pid-cursor"
        cursor.write_text(f"{PID - 1}\n")
        # Check the isolated procfs write before starting the daemon.
        PID_CURSOR.write_text(PID_CURSOR.read_text())
        launcher = directory / "claude"
        launcher.write_text(f"#!{sys.executable}\n{LAUNCHER}")
        launcher.chmod(0o700)
        terminfo = directory / "terminfo"
        subprocess.run(
            ["tic", "-x", "-o", terminfo, terminfo_source], check=True, timeout=5
        )
        os.environ["TERM"] = "xterm-256color"
        environment = os.environ | {
            "PATH": f"{directory}:{os.environ['PATH']}",
            "SHELL": "/bin/sh",
            "TERMINFO": str(terminfo),
            "CORVID_TEST_PID_CURSOR": str(cursor),
        }
        command = [binary.resolve(), "--runtime-dir", directory / "runtime"]
        daemon = subprocess.Popen([*command, "daemon", "--state-dir", directory / "state"], env=environment)
        try:
            wait_for_daemon(command, daemon)
            companion = Companion(command, directory)
            try:
                companion.start()
                verify(command, companion, directory)
            finally:
                companion.close()
        finally:
            stop(daemon)
    print(f"Reused PID {PID} in the same pane and slot. Verified that only the new capture focuses it.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("terminfo", type=Path)
    args = parser.parse_args()
    run(args.binary, args.terminfo)
