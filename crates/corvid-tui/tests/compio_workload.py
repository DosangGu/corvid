"""Compare two release binaries on Linux with four PTYs and eight snapshot watchers."""

import argparse
from contextlib import ExitStack
import json
import math
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid

from interactive import OuterTerminal, stop, wait_for_daemon
from ssh_lifecycle import transcode

SCHEMA = Path(__file__).resolve().parents[2] / "corvid-protocol/proto/corvid.proto"
CLOCK_TICKS = os.sysconf("SC_CLK_TCK")
MAX_FRAME = 256 * 1024
PING_COUNT = 20_000
INPUT_COUNT = 128
TERMINALS = 4
WATCHERS = 8


def frame(payload):
    return struct.pack(">IB", len(payload) + 1, 1) + payload


def read_frame(incoming):
    header = incoming.read(5)
    if len(header) != 5:
        raise RuntimeError("daemon closed before completing a frame")
    length, kind = struct.unpack(">IB", header)
    if kind != 1 or not 1 <= length <= MAX_FRAME:
        raise RuntimeError("unexpected control frame")
    payload = incoming.read(length - 1)
    if len(payload) != length - 1:
        raise RuntimeError("daemon closed inside a control payload")
    return payload


def connect(stack, runtime, capability):
    peer = stack.enter_context(socket.socket(socket.AF_UNIX, socket.SOCK_STREAM))
    peer.settimeout(5)
    peer.connect(str(runtime / "control.sock"))
    incoming = stack.enter_context(peer.makefile("rb"))
    identity = uuid.uuid4()
    greeting = encode(
        'protocol_major:1 request_id:"hello" '
        f'hello{{controller_id:"{identity}" supported_major_versions:1 '
        f'required_capabilities:"{capability}"}}'
    )
    peer.sendall(b"CRV1" + frame(greeting))
    if incoming.read(4) != b"CRV1":
        raise RuntimeError("unexpected control preamble")
    reply = decode(read_frame(incoming))
    if not re.search(r"^hello_result\s*{", reply, re.M):
        raise RuntimeError("daemon rejected the qualification client")
    scope = " ".join(
        re.findall(r"^(?:protocol_major|protocol_minor|host_id|server_epoch):[^\n]+", reply, re.M)
    )
    return peer, incoming, scope


def encode(message):
    return transcode(SCHEMA, "encode", message.encode())


def decode(payload):
    return transcode(SCHEMA, "decode", payload).decode()


def watch(stack, runtime):
    peer, incoming, scope = connect(stack, runtime, "snapshot_watch.v1")
    peer.sendall(frame(encode(f'{scope} request_id:"watch" snapshot_watch{{}}')))
    while True:
        reply = decode(read_frame(incoming))
        if re.search(r"^watch_ready\s*{", reply, re.M):
            return
        if re.search(r"^error\s*{", reply, re.M):
            raise RuntimeError("daemon failed to establish a snapshot watch")


def metrics(process):
    directory = Path(f"/proc/{process.pid}")
    stat = (directory / "stat").read_text().rsplit(")", 1)[1].split()
    ticks = int(stat[11]) + int(stat[12])
    switches = 0
    for thread in (directory / "task").iterdir():
        try:
            status = (thread / "status").read_text()
        except FileNotFoundError:
            continue
        switches += sum(
            int(value) for value in re.findall(r"^(?:non)?voluntary_ctxt_switches:\s+(\d+)", status, re.M)
        )
    return ticks, switches


def sample(daemon, terminals, energy_counter):
    attachments = [metrics(terminal.process) for terminal in terminals]
    energy = None if energy_counter is None else int(energy_counter.read_text())
    return metrics(daemon), tuple(sum(values) for values in zip(*attachments)), energy


def difference(before, after, energy_range):
    result = {
        group: {
            "cpu_seconds": round((last[0] - first[0]) / CLOCK_TICKS, 4),
            "context_switches": last[1] - first[1],
        }
        for group, first, last in zip(("daemon", "attached_clients"), before, after)
    }
    if energy_range is not None:
        result["package_energy_joules"] = (after[2] - before[2]) % energy_range / 1_000_000
    return result


def ping(peer, incoming, scope):
    payload = frame(encode(f'{scope} request_id:"ping" ping{{}}'))
    peer.sendall(payload)
    expected = read_frame(incoming)
    if not re.search(r"^pong\s*{", decode(expected), re.M):
        raise RuntimeError("daemon did not reply to Ping")
    times = []
    for _ in range(PING_COUNT):
        start = time.monotonic_ns()
        peer.sendall(payload)
        reply = read_frame(incoming)
        times.append(time.monotonic_ns() - start)
        if reply != expected:
            raise RuntimeError("daemon returned a different Ping reply")
    return p95(times, 1_000)


def input_rounds(terminals):
    times = []
    for index in range(INPUT_COUNT):
        terminal = terminals[index % len(terminals)]
        marker = b"AAAAAAAA" if (index // len(terminals)) % 2 == 0 else b"ZZZZZZZZ"
        terminal.output.clear()
        start = time.monotonic_ns()
        terminal.send(marker + b"\n")
        terminal.expect(marker)
        times.append(time.monotonic_ns() - start)
    return p95(times, 1_000_000)


def p95(times, unit):
    return round(sorted(times)[math.ceil(0.95 * len(times)) - 1] / unit, 3)


def run(binary, directory, idle_seconds, energy_counter, energy_range):
    runtime = directory / "run"
    command = [binary, "--runtime-dir", runtime]
    environment = os.environ | {"CORVID_VIEW_STATE_DIR": str(directory / "view"), "TERM": "xterm-256color"}
    daemon = subprocess.Popen([*command, "daemon", "--state-dir", directory / "state"], env=environment)
    with ExitStack() as stack:
        stack.callback(stop, daemon)
        wait_for_daemon(command, daemon)
        terminals = []
        for number in range(TERMINALS):
            shell = r'''stty -echo; printf 'BENCH-READY'; while IFS= read -r line; do printf '\033[2;1H%s' "$line"; done'''
            terminal = stack.enter_context(OuterTerminal(
                [*command, "attach", "--new-workspace", f"Bench {number}", "--", "/bin/sh", "-c", shell],
                env=environment,
            ))
            terminal.expect(b"BENCH-READY")
            terminals.append(terminal)
        for _ in range(WATCHERS):
            watch(stack, runtime)
        peer, incoming, scope = connect(stack, runtime, "ping.v1")
        before = sample(daemon, terminals, energy_counter)
        time.sleep(idle_seconds)
        idle = sample(daemon, terminals, energy_counter)
        latency_ping = ping(peer, incoming, scope)
        after_pings = sample(daemon, terminals, energy_counter)
        latency_input = input_rounds(terminals)
        after_input = sample(daemon, terminals, energy_counter)
        return {
            "idle_seconds": idle_seconds,
            "idle": difference(before, idle, energy_range),
            "ping_samples": PING_COUNT,
            "ping_p95_us": latency_ping,
            "ping": difference(idle, after_pings, energy_range),
            "input_samples": INPUT_COUNT,
            "input_redraw_p95_ms": latency_input,
            "input": difference(after_pings, after_input, energy_range),
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("compio", type=Path)
    parser.add_argument("--idle-seconds", type=int, choices=range(1, 21), default=15, metavar="1–20")
    parser.add_argument("--energy-counter", type=Path, help="readable package energy_uj file")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("qualification requires Linux /proc and Unix PTYs")
    energy_range = None
    if args.energy_counter is not None:
        try:
            int(args.energy_counter.read_text())
            energy_range = int(args.energy_counter.with_name("max_energy_range_uj").read_text())
        except (OSError, ValueError) as error:
            parser.error(f"cannot read the package energy counter: {error}")
    binaries = [("baseline", args.baseline.resolve()), ("compio", args.compio.resolve())]
    with tempfile.TemporaryDirectory(prefix=".corvid-runtime-bench-", dir=Path.home()) as temporary:
        for index, (name, binary) in enumerate(binaries + binaries[::-1]):
            directory = Path(temporary) / str(index)
            directory.mkdir(mode=0o700)
            report = run(binary, directory, args.idle_seconds, args.energy_counter, energy_range)
            print(json.dumps({"backend": name, **report}), flush=True)


if __name__ == "__main__":
    main()
