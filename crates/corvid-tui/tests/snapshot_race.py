"""Pause a real snapshot watch during transfer and inspect its catch-up frames."""

from contextlib import contextmanager
from dataclasses import dataclass
import json
from pathlib import Path
import re
import select
import socket
import struct
import subprocess
import uuid

from companion_fixture import fields
from interactive import stop
from ssh_lifecycle import transcode


SCHEMA = Path(__file__).resolve().parents[2] / "corvid-protocol/proto/corvid.proto"
PREAMBLE = b"CRV1"
HISTORY_RUNS = 80
REQUESTS_PER_RUN = 256


def new_id():
    return str(uuid.uuid4())


@dataclass(frozen=True)
class Pane:
    workspace: str
    tab: str
    pane: str

    def layout(self, label):
        return {
            "id": self.tab, "label": label,
            "root": {"kind": "pane", "id": self.pane, "last_cwd": None},
        }


def seed_history(directory):
    agent = Pane(new_id(), new_id(), new_id())
    shell = Pane(agent.workspace, new_id(), new_id())
    destination = Pane(new_id(), new_id(), new_id())
    workspaces = [
        {"id": agent.workspace, "label": "Source", "tabs": [agent.layout("Agent"), shell.layout("Shell")]},
        {"id": destination.workspace, "label": "Destination", "tabs": [destination.layout("Shell")]},
    ]
    evidence = {
        "source": "provider_hook", "source_version": "fixture", "observed_unix_ms": 1,
        "confidence": "confirmed", "coverage_generation": 1,
    }
    epoch = new_id()
    history = []
    for _ in range(HISTORY_RUNS):
        history.append({
            "epoch": epoch, "run": new_id(), "terminal": new_id(),
            "location": {"workspace": agent.workspace, "tab": agent.tab, "pane": agent.pane},
            "provider": "claude", "lifecycle": "live", "process": "running", "activity": "working",
            "evidence": evidence, "attention": [],
            "requests": [
                {"id": new_id(), "kind": "question", "status": "unverified"}
                for _ in range(REQUESTS_PER_RUN)
            ],
        })
    state = directory / "state"
    state.mkdir(mode=0o700)
    saved = state / "state.json"
    saved.touch(mode=0o600)
    saved.write_text(json.dumps({
        "schema_version": 2, "layout": {"workspaces": workspaces}, "history": history,
    }), encoding="utf-8")
    return agent, shell, destination.workspace


def read_exact(peer, length):
    received = bytearray()
    while len(received) < length:
        data = peer.recv(length - len(received))
        assert data, "snapshot peer closed during a frame"
        received.extend(data)
    return bytes(received)


def forward_frame(sender, receiver):
    header = read_exact(sender, 5)
    length, kind = struct.unpack(">IB", header)
    assert kind == 1 and 1 <= length <= 256 * 1024
    payload = read_exact(sender, length - 1)
    receiver.sendall(header + payload)
    return payload


def number(frame, name):
    match = re.search(rf"^[ \t]*{name}: (\d+)$", frame, re.MULTILINE)
    assert match is not None, f"snapshot field {name} is missing"
    return int(match[1])


def entity(frame, kind, field, value):
    blocks = re.findall(rf"^    {kind} \{{\n(.*?)^    \}}$", frame, re.MULTILINE | re.DOTALL)
    return next((block for block in blocks if f'{field}: "{value}"' in block), None)


def check_location(frame, agent, pane):
    run = entity(frame, "agent", "agent_run_id", agent["run"])
    terminal = entity(frame, "terminal", "terminal_id", agent["terminal"])
    assert run is not None and terminal is not None
    for item in (run, terminal):
        for field in ("workspace", "tab", "pane"):
            assert f'{field}_id: "{getattr(pane, field)}"' in item, item
    assert f'terminal_id: "{agent["terminal"]}"' in run


class PausedWatch:
    def __init__(self, upstream, client, watcher):
        self.upstream = upstream
        self.client = client
        self.watcher = watcher
        begin = self.next()
        assert "snapshot_begin {" in begin, begin
        self.base = number(begin, "sequence")
        self.chunks = number(begin, "chunks")
        assert self.chunks > 1, "the fixture did not load its large snapshot"

    def next(self):
        payload = forward_frame(self.upstream, self.client)
        return transcode(SCHEMA, "decode", payload).decode()

    def finish(self, source, destination, agent, question, expected):
        assert not select.select([self.watcher.stdout], [], [], 0)[0], "watch published before catch-up"
        for index in range(self.chunks):
            frame = self.next()
            assert "snapshot_chunk {" in frame and number(frame, "sequence") == self.base
            assert f'request_id: "{question["request_id"]}"' not in frame
            if index == 0:
                check_location(frame, agent, source)
        end = self.next()
        assert "snapshot_end {" in end and number(end, "sequence") == self.base

        question_sequence, move_sequence, ready = self.catch_up(source, destination, agent, question)
        assert self.base < question_sequence < move_sequence <= int(expected["sequence"]) <= ready
        assert select.select([self.watcher.stdout], [], [], 5)[0], "Rust watch did not become ready"
        view = fields(self.watcher.stdout.readline())
        assert int(view["sequence"]) == ready
        for name in ("host_id", "server_epoch", "workspaces", "terminals", "clients", "agents", "requests", "attention"):
            assert view[name] == expected[name], (name, view, expected)
        print(
            f"Snapshot {self.base} in {self.chunks} chunks, question {question_sequence}, "
            f"move {move_sequence}, ready {ready}.",
            flush=True,
        )

    def catch_up(self, source, destination, agent, question):
        sequence = self.base
        question_sequence = None
        move_sequence = None
        while True:
            frame = self.next()
            if "watch_ready {" in frame:
                assert number(frame, "sequence") == sequence
                assert question_sequence is not None and move_sequence is not None
                return question_sequence, move_sequence, sequence

            assert "snapshot_delta {" in frame, frame
            sequence += 1
            assert number(frame, "sequence") == sequence
            request = entity(frame, "request", "request_id", question["request_id"])
            if request is not None:
                assert f'agent_run_id: "{agent["run"]}"' in request
                assert "INPUT_REQUEST_STATUS_PENDING" in request
                assert entity(frame, "attention", "attention_id", question["attention_id"]) is not None
                assert question_sequence is None
                question_sequence = sequence

            moved = entity(frame, "workspace", "workspace_id", destination)
            if moved is not None and f'tab_id: "{source.tab}"' in moved:
                previous = entity(frame, "workspace", "workspace_id", source.workspace)
                assert previous is not None and f'tab_id: "{source.tab}"' not in previous
                check_location(frame, agent, Pane(destination, source.tab, source.pane))
                assert move_sequence is None
                move_sequence = sequence


@contextmanager
def paused_watch(command, directory):
    runtime = directory / "watch-proxy"
    runtime.mkdir(mode=0o700)
    with socket.socket(socket.AF_UNIX) as listener, socket.socket(socket.AF_UNIX) as upstream:
        path = runtime / "control.sock"
        listener.bind(str(path))
        path.chmod(0o600)
        listener.listen(1)
        listener.settimeout(5)
        watcher = subprocess.Popen(
            [command[0], "--runtime-dir", runtime, "control", "watch"], stdout=subprocess.PIPE, text=True,
        )
        try:
            client, _ = listener.accept()
            with client:
                client.settimeout(5)
                upstream.settimeout(5)
                upstream.connect(str(directory / "run/control.sock"))
                for sender, receiver in ((upstream, client), (client, upstream)):
                    assert read_exact(sender, 4) == PREAMBLE
                    receiver.sendall(PREAMBLE)
                forward_frame(client, upstream)
                forward_frame(upstream, client)
                forward_frame(client, upstream)
                yield PausedWatch(upstream, client, watcher)
        finally:
            stop(watcher)
