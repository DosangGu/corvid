"""Exercise newer client binaries against a historical Corvid daemon."""

import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from interactive import OuterTerminal, stop, wait_for_daemon


PROCESS = rb"PID=(\d+) TICK=(\d+) SIZE=80x24"


def inventory(command, request):
    return subprocess.check_output([*command, "control", request], timeout=3)


def reject_attachment(command, options, error, cols=80, rows=24):
    with OuterTerminal([*command, "attach", *options], cols=cols, rows=rows) as terminal:
        terminal.expect(error)
        terminal.wait(1)


def reject_incompatible_clients(compatible, incompatible, workspace):
    result = subprocess.run(
        [*incompatible, "control", "host"], capture_output=True, timeout=5,
    )
    assert result.returncode == 1 and not result.stdout
    assert b"ProtocolMismatch" in result.stderr
    assert b"supported protocol majors: [1]" in result.stderr and b"manually" in result.stderr
    reject_attachment(incompatible, ["--new-workspace", "Incompatible"], b"ProtocolMismatch")
    reject_attachment(compatible, ["--claim-workspace", workspace], b"UnsupportedCapability")
    reject_attachment(compatible, [], b"running Corvid daemon supports up to", cols=319, rows=80)


def verify_retained_terminal(old, compatible, incompatible, daemon, progress):
    identity = inventory(old, "host")
    layout = inventory(old, "workspaces")
    workspace = layout.splitlines()[0].split()[1].decode()
    fixture = Path(__file__).with_name("interactive.py")
    start = [*old, "attach", "--", sys.executable, fixture, "--child", progress]

    with OuterTerminal(start) as original:
        initial = original.expect(PROCESS)
        assert inventory(compatible, "host") == identity
        active = inventory(old, "clients")

        reject_incompatible_clients(compatible, incompatible, workspace)
        assert daemon.poll() is None and original.process.poll() is None
        assert inventory(old, "host") == identity
        assert inventory(old, "workspaces") == layout
        assert inventory(old, "clients") == active
        original.send(b"s")
        original.expect(b"MEASURED=80x24")
        original.send(b"\x1dd")
        original.wait(0)

    deadline = time.monotonic() + 5
    while inventory(old, "clients"):
        assert time.monotonic() < deadline, "old writer did not detach"
        time.sleep(0.02)

    with OuterTerminal([*compatible, "attach"]) as newer:
        retained = newer.expect(PROCESS)
        assert retained[1] == initial[1] and int(retained[2]) > int(initial[2])
        newer.send(b"q")
        newer.expect(b"TERMINAL-EXIT")
        newer.wait(7)


def run(old, compatible, incompatible):
    root = Path(__file__).resolve().parents[3]
    profile = root / "spikes/terminal/corvid.terminfo"
    with tempfile.TemporaryDirectory(prefix="corvid-skew-", dir=root) as temporary:
        directory = Path(temporary)
        database = directory / "terminfo"
        subprocess.run(["tic", "-x", "-o", database, profile], check=True, timeout=5)
        runtime = directory / "runtime"
        old, compatible, incompatible = (
            [binary.resolve(), "--runtime-dir", runtime] for binary in (old, compatible, incompatible)
        )
        daemon = subprocess.Popen(
            [*old, "daemon", "--state-dir", directory / "state"],
            env=os.environ | {"TERMINFO": str(database)},
        )
        try:
            wait_for_daemon(old, daemon)
            verify_retained_terminal(old, compatible, incompatible, daemon, directory / "progress")
        finally:
            stop(daemon)
    print("Compatible minor negotiation, major and capability rejection, and same-PTY reattach verified.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("old", type=Path, help="historical daemon and client with protocol 1.0")
    parser.add_argument("compatible", type=Path, help="current binary built with a test minor of 1")
    parser.add_argument("incompatible", type=Path, help="current binary built with a test major of 2")
    arguments = parser.parse_args()
    run(arguments.old, arguments.compatible, arguments.incompatible)
