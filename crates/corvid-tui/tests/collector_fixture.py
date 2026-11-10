"""Inject a collector failure while retaining the provider's exec-form ancestry."""

import json
import os
from pathlib import Path
import signal
import socket
import sys
import time

MAX_CONNECTIONS = 32
SATURATION = "32 admitted, next refused"
SCRIPT = Path(__file__).resolve()


def record(trace, event):
    with trace.open("a") as stream:
        print(event, file=stream)


def verify_crashed_hooks(trace, expected, *, optional=frozenset()):
    actual = set(trace.read_text().splitlines())
    missing = sorted(expected - actual)
    unexpected = sorted(actual - expected - optional)
    assert not missing and not unexpected, f"missing crashes {missing}, unexpected crashes {unexpected}"


def saturate(trace, command):
    path = str(Path(os.environ["CORVID_RUNTIME_DIR"]) / "collector.sock")
    peers = []
    for _ in range(MAX_CONNECTIONS):
        peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        peer.settimeout(0.03)
        peer.connect(path)
        assert peer.recv(4, socket.MSG_WAITALL) == b"CRV1"
        os.set_inheritable(peer.fileno(), True)
        peers.append(peer)

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as overflow:
        overflow.settimeout(0.03)
        overflow.connect(path)
        try:
            assert overflow.recv(4, socket.MSG_WAITALL) == b"", "overflow was admitted"
        except ConnectionResetError:
            pass
    record(trace, SATURATION)
    # The admitted sockets stay open through exec and close with the observer.
    os.execv(command[0], command)


def corrupt_stdin(trace):
    payload = sys.stdin.buffer.read(64 * 1024 + 1)
    assert len(payload) <= 64 * 1024
    source = json.loads(payload)
    assert source["hook_event_name"] == "PostToolUse" and source["tool_use_id"]
    invalid = {key: source[key] for key in ("hook_event_name", "session_id", "prompt_id", "tool_name")}
    invalid["tool_use_id"] = ""
    descriptor = os.memfd_create("corvid-invalid-hook", os.MFD_CLOEXEC)
    try:
        os.write(descriptor, json.dumps(invalid).encode())
        os.lseek(descriptor, 0, os.SEEK_SET)
        os.dup2(descriptor, sys.stdin.fileno())
    finally:
        os.close(descriptor)
    record(trace, "invalid tool-use ID")


def main():
    fault, log, *command = sys.argv[1:]
    trace = Path(log)
    if fault == "timeout":
        record(trace, f"started {os.getpid()}")
        time.sleep(3)
        record(trace, "finished")
        return

    if fault == "saturate":
        saturate(trace, command)
    elif fault == "malformed":
        corrupt_stdin(trace)
        os.execv(command[0], command)
    elif fault == "crash":
        event, = command
        record(trace, event)
        os.kill(os.getpid(), signal.SIGKILL)
    else:
        raise ValueError(f"unknown collector fixture fault {fault!r}")


if __name__ == "__main__":
    main()
