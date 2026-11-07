"""Capture allowlisted hook fields from Claude 2.1.318 with a loopback API fixture."""

import argparse
import fcntl
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

from interactive import stop

VERSION = "2.1.318 (Claude Code)"
HOOKS = (
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PermissionRequest",
    "PostToolUse", "PostToolUseFailure", "Stop", "StopFailure", "SessionEnd",
)
IDS = ("session_id", "prompt_id", "tool_use_id", "agent_id")
CAPTURE = """
import json
from pathlib import Path
import sys

event = json.load(sys.stdin)
fields = (
    "hook_event_name", "source", "reason", "tool_name", "session_id",
    "prompt_id", "tool_use_id", "agent_id", "is_interrupt",
)
with Path(sys.argv[1]).open("a") as trace:
    print(json.dumps({field: event[field] for field in fields if field in event}), file=trace)
"""


class ModelFixture(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path.startswith("/v1/messages/count_tokens"):
            self.reply(b'{"input_tokens": 123}', "application/json")
            return
        if not self.path.startswith("/v1/messages"):
            self.send_error(404)
            return

        offered_tools = {tool.get("name") for tool in request.get("tools", [])}
        question = "AskUserQuestion" in offered_tools and not self.server.question_sent
        self.server.question_sent |= question
        self.server.message_count += 1
        message = {
            "id": f"msg_corvid_{self.server.message_count}",
            "type": "message",
            "role": "assistant",
            "model": request["model"],
            "content": [],
            "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 123, "output_tokens": 1},
        }
        block, delta, reason = response_content(question)
        events = (
            {"type": "message_start", "message": message},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": reason, "stop_sequence": None},
             "usage": {"output_tokens": 10}},
            {"type": "message_stop"},
        )
        body = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events)
        self.reply(body.encode(), "text/event-stream")

    def reply(self, body, content_type):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def response_content(question):
    if not question:
        return (
            {"type": "text", "text": ""},
            {"type": "text_delta", "text": "Fixture response."},
            "end_turn",
        )
    block = {
        "type": "tool_use",
        "name": "AskUserQuestion",
        "id": "toolu_corvid_fixture",
        "input": {},
    }
    question = {
        "question": "Which fixture option do you choose?",
        "header": "Fixture",
        "options": [
            {"label": "Alpha", "description": "First fixture option"},
            {"label": "Beta", "description": "Second fixture option"},
        ],
        "multiSelect": False,
    }
    delta = {"type": "input_json_delta", "partial_json": json.dumps({"questions": [question]})}
    return block, delta, "tool_use"


def configure(directory, port):
    home = directory / "home"
    config = home / ".claude"
    config.mkdir(parents=True)
    capture = directory / "capture.py"
    capture.write_text(CAPTURE)
    trace = directory / "events.jsonl"
    hook = shlex.join([sys.executable, str(capture), str(trace)])
    handler = {"type": "command", "command": hook, "timeout": 1}
    settings = {"hooks": {event: [{"hooks": [handler]}] for event in HOOKS}}
    (config / "settings.json").write_text(json.dumps(settings))
    environment = {
        "HOME": str(home),
        "PATH": os.defpath,
        "CLAUDE_CONFIG_DIR": str(config),
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{port}",
        "ANTHROPIC_API_KEY": "corvid-loopback-fixture",
        "TERM": "xterm-256color",
        "API_TIMEOUT_MS": "1500",
        "CLAUDE_CODE_MAX_RETRIES": "0",
    }
    return trace, environment


class FixtureTerminal:
    def __init__(self, process, master, trace):
        self.process = process
        self.master = master
        self.trace = trace
        self.screen = b""
        self.deadline = time.monotonic() + 20

    def wait(self, condition, description):
        while not condition():
            assert self.process.poll() is None, f"Claude exited before {description}"
            assert time.monotonic() < self.deadline, f"timed out waiting for {description}"
            if select.select([self.master], [], [], 0.05)[0]:
                self.screen += os.read(self.master, 8192)
                assert len(self.screen) <= 256 * 1024, "fixture screen exceeded its bound"

    def expect(self, text):
        self.wait(lambda: text in self.text(), text.decode())

    def text(self):
        positioned = re.sub(rb"\x1b\[[0-9;]*G", b" ", self.screen)
        plain = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", positioned)
        return b" ".join(plain.split())

    def send(self, data):
        if len(data) > 1 and data.endswith(b"\r"):
            time.sleep(0.25)
            os.write(self.master, data[:-1])
            time.sleep(0.2)
            data = b"\r"
        os.write(self.master, data)

    def events(self):
        if not self.trace.exists():
            return []
        contents = self.trace.read_bytes()
        end = contents.rfind(b"\n")
        return [json.loads(line) for line in contents[:end + 1].splitlines()]

    def event(self, **match):
        self.wait(
            lambda: any(all(event.get(key) == value for key, value in match.items())
                        for event in self.events()),
            ", ".join(match.values()),
        )


def exercise(terminal, scenario):
    for screen, answer in (
        (b"Select with numbers [1-7]", b"2\r"),
        (b"Enter y/n:", b"y\r"),
    ):
        terminal.expect(screen)
        terminal.send(answer)
    security = b"Press Enter to continue"
    trust = b"Is this a project you created"
    terminal.wait(
        lambda: security in terminal.text() or trust in terminal.text(),
        "workspace setup",
    )
    if security in terminal.text() and trust not in terminal.text():
        terminal.send(b"\r")
    terminal.expect(trust)
    terminal.send(b"y\r")
    terminal.event(hook_event_name="SessionStart", source="startup")
    terminal.send(b"Corvid hook schema fixture\r")
    terminal.expect(b"First fixture option")
    terminal.send(b"1\r")
    terminal.event(hook_event_name="PostToolUse", tool_name="AskUserQuestion")
    terminal.event(hook_event_name="Stop")
    if scenario == "clear":
        terminal.send(b"/clear\r")
        terminal.event(hook_event_name="SessionStart", source="clear")
    terminal.send(b"/exit\r")
    assert terminal.process.wait(timeout=5) == 0, "fixture Claude exit failed"
    return terminal.events()


def capture(binary, directory, port, scenario):
    trace, environment = configure(directory, port)
    project = directory / "project"
    project.mkdir()
    master, slave = pty.openpty()
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))
        args = [
            "setsid", "--ctty", "--wait", str(binary), "--model", "sonnet",
            "--strict-mcp-config", "--no-chrome", "--ax-screen-reader",
            "--permission-mode", "manual",
        ]
        process = subprocess.Popen(
            args, env=environment, cwd=project, stdin=slave, stdout=slave, stderr=slave,
        )
        try:
            return exercise(FixtureTerminal(process, master, trace), scenario)
        finally:
            stop(process)
    finally:
        os.close(master)
        os.close(slave)


def sanitize(events):
    ids = {}
    for event in events:
        for field in IDS:
            if field in event:
                value = event[field]
                if value not in ids:
                    index = len(ids) + 1
                    if field == "tool_use_id":
                        ids[value] = f"toolu_fixture_{index}"
                    else:
                        ids[value] = f"00000000-0000-4000-8000-{index:012d}"
                event[field] = ids[value]
        print(json.dumps(event, separators=(",", ":")))


def verify(events, scenario):
    first = events[0]
    assert first["hook_event_name"] == "SessionStart"
    assert first["source"] == "startup"
    assert "prompt_id" not in first
    session = first["session_id"]
    submitted = next(event for event in events if event["hook_event_name"] == "UserPromptSubmit")
    turn = submitted["prompt_id"]
    before = next(event for event in events if event["hook_event_name"] == "PreToolUse")
    after = next(event for event in events if event["hook_event_name"] == "PostToolUse")
    permission = next(event for event in events if event["hook_event_name"] == "PermissionRequest")
    response = next(event for event in events if event["hook_event_name"] == "Stop")
    for event in (submitted, before, after, permission, response):
        assert event["session_id"] == session
        assert event["prompt_id"] == turn
    assert before["tool_name"] == after["tool_name"] == permission["tool_name"] == "AskUserQuestion"
    assert before["tool_use_id"] == after["tool_use_id"]
    assert "tool_use_id" not in permission
    assert events[-1]["hook_event_name"] == "SessionEnd"
    assert events[-1]["reason"] == "prompt_input_exit"
    if scenario == "clear":
        ended = next(event for event in events if event.get("reason") == "clear")
        started = next(event for event in events if event.get("source") == "clear")
        assert ended["hook_event_name"] == "SessionEnd" and ended["session_id"] == session
        assert started["hook_event_name"] == "SessionStart" and started["session_id"] != session
        assert events[-1]["session_id"] == started["session_id"]
    else:
        assert events[-1]["session_id"] == session


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", choices=("answer", "clear"), default="answer")
    args = parser.parse_args()
    executable = shutil.which("claude")
    if executable is None:
        parser.error("claude must be installed on PATH")
    binary = Path(executable).resolve()
    version = subprocess.check_output([binary, "--version"], text=True, timeout=3).strip()
    assert version == VERSION, f"expected {VERSION}, found {version}"
    with tempfile.TemporaryDirectory(prefix="corvid-claude-") as directory:
        with HTTPServer(("127.0.0.1", 0), ModelFixture) as server:
            server.question_sent = False
            server.message_count = 0
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                events = capture(binary, Path(directory), server.server_port, args.scenario)
            finally:
                server.shutdown()
                thread.join()
    verify(events, args.scenario)
    sanitize(events)


if __name__ == "__main__":
    main()
