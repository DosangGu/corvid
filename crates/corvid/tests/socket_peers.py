"""Check both Unix peer credentials in a disposable two-UID environment."""

import argparse
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile

from interactive import OuterTerminal, stop, wait_for_daemon


SOCKETS = ("client.sock", "control.sock", "collector.sock")

FOREIGN_CONNECT = """
import socket
import sys
from pathlib import Path

runtime, expected, *names = sys.argv[1:]
for name in names:
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(3)
        try:
            client.connect(str(Path(runtime) / name))
        except PermissionError:
            assert expected == 'permission-denied'
        else:
            assert expected == 'closed'
            assert client.recv(4) == b''
"""

FOREIGN_LISTEN = """
import socket
import sys

with socket.socket(fileno=int(sys.argv[1])) as server:
    server.settimeout(5)
    server.listen()
    print('READY', flush=True)
    peer, _ = server.accept()
    with peer:
        peer.settimeout(3)
        assert peer.recv(4) == b''
"""


def foreign_connect(runtime, uid, expected, rounds=1):
    subprocess.run(
        [sys.executable, "-c", FOREIGN_CONNECT, runtime, expected, *(SOCKETS * rounds)],
        user=uid, group=uid, check=True, timeout=3,
    )


def require_foreign_rejection(command, directory, runtime, uid):
    fixture = Path(__file__).with_name("interactive.py")
    progress = directory / "progress"
    attach = [*command, "attach", "--", sys.executable, fixture, "--child", progress]
    host = subprocess.check_output([*command, "control", "host"], timeout=3)

    with OuterTerminal(attach) as terminal:
        terminal.expect(rb"PID=\d+ TICK=\d+ SIZE=80x24")
        before = subprocess.check_output([*command, "control", "clients"], timeout=3)
        foreign_connect(runtime, uid, "permission-denied")

        # Allow traversal only within this fixture to exercise SO_PEERCRED.
        try:
            directory.chmod(0o711)
            runtime.chmod(0o711)
            for name in SOCKETS:
                (runtime / name).chmod(0o666)
            foreign_connect(runtime, uid, "closed", rounds=64)
            with socket.socket(socket.AF_UNIX) as owner:
                owner.settimeout(3)
                owner.connect(str(runtime / "control.sock"))
                with owner.makefile("rb") as frames:
                    assert frames.read(4) == b"CRV1"
        finally:
            for name in SOCKETS:
                (runtime / name).chmod(0o600)
            runtime.chmod(0o700)
            directory.chmod(0o700)

        assert subprocess.check_output([*command, "control", "clients"], timeout=3) == before
        assert subprocess.check_output([*command, "control", "host"], timeout=3) == host
        terminal.send(b"s")
        terminal.expect(b"MEASURED=80x24")
        terminal.send(b"q")
        terminal.expect(b"TERMINAL-EXIT")
        terminal.wait(7)


def reject_foreign_listener(binary, runtime, uid):
    runtime.mkdir(mode=0o700)
    path = runtime / "control.sock"
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(str(path))
        path.chmod(0o600)
        fd = listener.fileno()
        process = subprocess.Popen(
            [sys.executable, "-c", FOREIGN_LISTEN, str(fd)], pass_fds=(fd,),
            user=uid, group=uid, stdout=subprocess.PIPE,
        )
        try:
            assert process.stdout.readline() == b"READY\n"
            result = subprocess.run(
                [binary, "--runtime-dir", runtime, "control", "host"],
                capture_output=True, timeout=3,
            )
            assert result.returncode == 1 and not result.stdout
            assert b"unauthorized Unix peer UID" in result.stderr
            assert process.wait(timeout=3) == 0
        finally:
            stop(process)


def run(binary, uid):
    assert os.geteuid() == 0 and uid > 0, "use a disposable environment with root and another UID"
    binary = binary.resolve()
    with tempfile.TemporaryDirectory(prefix="corvid-uid-", dir="/run") as temporary:
        directory = Path(temporary)
        runtime = directory / "runtime"
        command = [binary, "--runtime-dir", runtime]
        daemon = subprocess.Popen([*command, "daemon", "--state-dir", directory / "state"])
        try:
            wait_for_daemon(command, daemon)
            require_foreign_rejection(command, directory, runtime, uid)
            assert daemon.poll() is None
            reject_foreign_listener(binary, directory / "foreign", uid)
        finally:
            stop(daemon)
    print("Filesystem and two-way Unix peer UID rejection verified with a live PTY.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corvid", type=Path)
    parser.add_argument("--foreign-uid", type=int, default=1)
    args = parser.parse_args()
    run(args.corvid, args.foreign_uid)
