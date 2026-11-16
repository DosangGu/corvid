"""Read and retire only the automatic monitors belonging to a Desktop fixture."""

import json
import subprocess
import time


def records(state):
    return [*state.glob("monitor/connection.json"),
            *state.glob("slot-board/*/monitor/connection.json")]


def command(binary, record):
    connection = json.loads(record.read_text())
    return [binary, "companion", "--socket-dir", connection["address"]]


def ready(binary, state, environment):
    deadline = time.monotonic() + 10
    while True:
        for record in records(state):
            reader = command(binary, record)
            reply = subprocess.run([*reader, "slots"], env=environment, text=True,
                                   capture_output=True, timeout=8)
            if reply.returncode == 0:
                header = dict(field.split("=", 1) for field in reply.stdout.splitlines()[0].split()
                              if "=" in field)
                if header.get("connection") == "Ready":
                    return reader, header["companion_epoch"]
        assert time.monotonic() < deadline, "Desktop did not start its agent monitor"
        time.sleep(0.1)


def assert_same(reader, epoch, environment):
    reply = subprocess.check_output([*reader, "slots"], env=environment, text=True, timeout=8)
    header = dict(field.split("=", 1) for field in reply.splitlines()[0].split() if "=" in field)
    assert header["connection"] == "Ready", header
    assert header["companion_epoch"] == epoch, "Closing Desktop restarted the monitor"


def stop(binary, state, environment):
    for record in records(state):
        reader = command(binary, record)
        subprocess.run([*reader, "stop"], env=environment, capture_output=True,
                       check=True, timeout=8)
        deadline = time.monotonic() + 5
        while subprocess.run([*reader, "slots"], env=environment, capture_output=True,
                             timeout=8).returncode == 0:
            assert time.monotonic() < deadline, "Fixture monitor did not stop"
            time.sleep(0.05)
