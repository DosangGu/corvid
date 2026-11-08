"""Capture allowlisted hook fields from Claude 2.1.318 with a loopback API fixture."""

import argparse
from contextlib import contextmanager
import fcntl
from http.server import BaseHTTPRequestHandler, HTTPServer, ThreadingHTTPServer
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

from collector_fixture import SATURATION, SCRIPT as COLLECTOR_FIXTURE, verify_crashed_hooks
from companion_fixture import Companion, assigned_slots, attention, fields, request_for
from interactive import stop, wait_for_daemon
from snapshot_race import paused_watch, seed_history

VERSION = "2.1.318 (Claude Code)"
HOOKS = (
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PermissionRequest",
    "PermissionDenied", "PostToolUse", "PostToolUseFailure", "Stop", "StopFailure", "SessionEnd",
    "SubagentStart", "SubagentStop", "PostToolBatch",
)
IDS = ("session_id", "prompt_id", "tool_use_id", "agent_id", "result_agent_id")
FAULT_TRACE = "collector-faults"
PLAN_REJECTION = "plan-rejection"
INPUT_TOOL_ID = "toolu_corvid_fixture"
OFFLINE_PROMPTS = (b"Corvid offline question one", b"Corvid offline question two")
PLAN_SCENARIOS = {"plan", "plan-rejected"}
SYNC_CHILD_SCENARIOS = {"child-sync", "child-nested"}
CHILD_SCENARIOS = SYNC_CHILD_SCENARIOS | {"child"}
CHILD_PROMPT = "Corvid fixture child reads the test note."
NESTED_PROMPT = "Corvid fixture grandchild reads the test note."
CAPTURE = """
import json
import os
from pathlib import Path
import sys

event = json.load(sys.stdin)
fields = (
    "hook_event_name", "source", "reason", "tool_name", "session_id",
    "prompt_id", "tool_use_id", "agent_id", "is_interrupt",
)
record = {field: event[field] for field in fields if field in event}
if event["hook_event_name"] in {"SubagentStart", "SubagentStop"}:
    record.update({field: event[field] for field in ("agent_type", "stop_hook_active") if field in event})
if event["hook_event_name"] == "PostToolUse" and event.get("tool_name") == "Agent":
    result = event.get("tool_response")
    if isinstance(result, dict):
        if isinstance(result.get("agentId"), str):
            record["result_agent_id"] = result["agentId"]
        if isinstance(result.get("status"), str):
            record["result_status"] = result["status"]
if event["hook_event_name"] == "PostToolBatch":
    record["tool_calls"] = [
        {key: call[key] for key in ("tool_name", "tool_use_id")}
        for call in event["tool_calls"] if call["tool_name"] in {"AskUserQuestion", "ExitPlanMode"}
    ]
if event["hook_event_name"] != "PostToolBatch" or record["tool_calls"]:
    with Path(sys.argv[1]).open("a") as trace:
        print(json.dumps(record), file=trace)
if len(sys.argv) > 2:
    helper, fault_trace = sys.argv[2:]
    os.execv(
        sys.executable,
        [sys.executable, "-B", helper, "crash", fault_trace, event["hook_event_name"]],
    )
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
        requested = self.server.fixture_tool
        if requested == "ExitPlanMode" and self.server.tool_sent and has_rejected_plan(request):
            self.server.plan_rejection.touch()
        tool = requested if requested in offered_tools and not self.server.tool_sent else None
        arguments = None
        if requested == "ExitPlanMode" and not self.server.plan_drafted:
            plan_root = re.escape(str(self.server.plan_directory))
            path = re.search(rf"{plan_root}/[\w-]+\.md", json.dumps(request.get("messages", [])))
            assert "Write" in offered_tools and path, "missing fixture plan file in plan-mode context"
            tool = "Write"
            arguments = {"file_path": path[0], "content": "# Fixture plan\n\nReturn a synthetic response.\n"}
            self.server.plan_drafted = True
        elif requested == "ExitPlanMode" and tool is None and not self.server.plan_search_sent:
            assert "ToolSearch" in offered_tools, "plan tool is absent and tool search is unavailable"
            tool = "ToolSearch"
            arguments = {"query": "select:ExitPlanMode"}
            self.server.plan_search_sent = True
        if self.server.offline_pair:
            tool = self.parallel_question(request, offered_tools)
        elif self.server.child_fixture:
            tool, arguments = self.child_response(request, offered_tools)
        else:
            self.server.tool_sent |= tool == requested
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
        block, delta, reason = response_content(tool, arguments)
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

    def parallel_question(self, request, offered_tools):
        if "AskUserQuestion" not in offered_tools:
            return None
        for prompt in OFFLINE_PROMPTS:
            if prompt not in self.server.questions_sent and any(
                prompt.decode() in text for text in user_texts(request)
            ):
                self.server.questions_sent.add(prompt)
                return "AskUserQuestion"
        return None

    def child_response(self, request, offered_tools):
        if not self.server.agent_sent:
            assert "Agent" in offered_tools
            self.server.agent_sent = True
            return "Agent", {
                "description": "Read fixture note",
                "prompt": CHILD_PROMPT,
                "subagent_type": "general-purpose",
            }

        texts = list(user_texts(request))
        from_child = any(CHILD_PROMPT in text for text in texts)
        from_nested = any(NESTED_PROMPT in text for text in texts)
        if self.server.nested_child and from_child and not from_nested:
            if not self.server.nested_agent_sent:
                assert "Agent" in offered_tools
                self.server.nested_agent_sent = True
                return "Agent", {
                    "description": "Delegate the fixture note",
                    "prompt": NESTED_PROMPT,
                    "subagent_type": "general-purpose",
                }
            self.wait_for_release(self.server.parent_release)
            return None, None
        if from_child or from_nested:
            if not self.server.child_read_sent.is_set():
                assert "Read" in offered_tools
                self.server.child_read_sent.set()
                return "Read", {"file_path": str(self.server.child_note), "limit": 1}
            self.wait_for_release(self.server.child_release)
            return None, None
        if self.server.parent_question_sent:
            return None, None

        assert self.server.child_read_sent.wait(5), "child never asked to read its fixture"
        assert "AskUserQuestion" in offered_tools
        self.server.parent_question_sent = True
        return "AskUserQuestion", None

    @staticmethod
    def wait_for_release(path):
        deadline = time.monotonic() + 8
        while not path.exists():
            assert time.monotonic() < deadline, f"fixture did not release {path.name}"
            time.sleep(0.02)

    def reply(self, body, content_type):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def user_texts(request):
    for message in request.get("messages", []):
        if message.get("role") != "user":
            continue
        content = message.get("content", [])
        if isinstance(content, str):
            yield content
        else:
            yield from (block.get("text", "") for block in content if block.get("type") == "text")


def has_rejected_plan(request):
    for message in request.get("messages", []):
        content = message.get("content", [])
        if message.get("role") != "user" or not isinstance(content, list):
            continue
        if any(
            block.get("type") == "tool_result"
            and block.get("tool_use_id") == INPUT_TOOL_ID
            and block.get("is_error") is True
            for block in content
        ):
            return True
    return False


def response_content(tool, arguments):
    if not tool:
        return (
            {"type": "text", "text": ""},
            {"type": "text_delta", "text": "Fixture response."},
            "end_turn",
        )
    block = {
        "type": "tool_use",
        "name": tool,
        "id": INPUT_TOOL_ID if tool in {"AskUserQuestion", "ExitPlanMode"}
              else f"toolu_corvid_{tool.lower()}",
        "input": {},
    }
    if tool == "AskUserQuestion":
        arguments = {"questions": [{
            "question": "Which fixture option do you choose?",
            "header": "Fixture",
            "options": [
                {"label": "Alpha", "description": "First fixture option"},
                {"label": "Beta", "description": "Second fixture option"},
            ],
            "multiSelect": False,
        }]}
    if arguments is None:
        arguments = {}
    delta = {"type": "input_json_delta", "partial_json": json.dumps(arguments)}
    return block, delta, "tool_use"


def configure(directory, port, corvid, fault):
    home = directory / "home"
    config = home / ".claude"
    config.mkdir(parents=True)
    capture = directory / "capture.py"
    capture.write_text(CAPTURE)
    trace = directory / "events.jsonl"
    hook = shlex.join([sys.executable, str(capture), str(trace)])
    handler = {"type": "command", "command": hook, "timeout": 1}
    settings = {"hooks": {event: [{"hooks": [handler]}] for event in HOOKS}}
    crash_hooks = set()
    if corvid is not None:
        generated = json.loads(subprocess.check_output([corvid, "hooks", "claude"], timeout=3))
        for event, handlers in generated["hooks"].items():
            if fault == "helper-crash":
                crash_hooks.add(event)
                # These child fixture records lie outside the production matchers.
                capture_tool = {"PreToolUse": "Agent", "PostToolUse": "Read"}.get(event)
                settings["hooks"][event] = []
                if capture_tool:
                    assert all(
                        re.search(group.get("matcher", ""), capture_tool) is None
                        for group in handlers
                    ), f"capture-only tool {capture_tool} overlaps a crashing {event} observer"
                    settings["hooks"][event].append({"matcher": f"^{capture_tool}$", "hooks": [handler]})
            for matcher in handlers:
                for observer in matcher["hooks"]:
                    if fault == "helper-crash":
                        observer.update(command=sys.executable, args=[
                            "-B", str(capture), str(trace), str(COLLECTOR_FIXTURE),
                            str(directory / FAULT_TRACE),
                        ])
                    else:
                        inject_fault(observer, event, fault, directory)
            settings["hooks"][event].extend(handlers)
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
    return trace, environment, crash_hooks


def inject_fault(observer, event, fault, directory):
    if fault == "missing-helper":
        observer["command"] = str(directory / "missing-observer")
        return
    if fault == "collector-saturated":
        mode = "saturate"
    elif fault == "timeout" and event == "Stop":
        mode = "timeout"
    elif fault == "malformed-result" and event == "PostToolUse":
        mode = "malformed"
    else:
        return

    command = [observer["command"], *observer["args"]]
    observer["command"] = sys.executable
    observer["args"] = [str(COLLECTOR_FIXTURE), mode, str(directory / FAULT_TRACE), *command]


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

    def event(self, *, after=0, **match):
        def matching():
            return next((event for event in self.events()[after:]
                         if all(event.get(key) == value for key, value in match.items())), None)

        self.wait(matching, ", ".join(match.values()))
        return matching()


def initialize_terminal(terminal):
    for screen, answer in (
        (b"Select with numbers [1-7]", b"2\r"),
        (b"Enter y/n:", b"y\r"),
    ):
        terminal.expect(screen)
        terminal.send(answer)
    security = b"Press Enter to continue"
    trust = b"Accessing workspace:"
    terminal.wait(
        lambda: security in terminal.text() or trust in terminal.text(),
        "workspace setup",
    )
    if security in terminal.text() and trust not in terminal.text():
        terminal.send(b"\r")
    terminal.expect(trust)
    terminal.send(b"y\r")
    return terminal.event(hook_event_name="SessionStart", source="startup")["session_id"]


def exercise(terminal, scenario, managed, fault):
    session = initialize_terminal(terminal)
    initial = managed.expect(terminal, collection="Healthy", process="Running") if managed else None
    current = initial
    terminal.send(b"Corvid hook schema fixture\r")
    child_start = terminal.event(hook_event_name="SubagentStart") if scenario in CHILD_SCENARIOS else None
    nested_start = None
    if scenario in SYNC_CHILD_SCENARIOS:
        nested_start = release_foreground_children(terminal, managed, child_start, nested=scenario == "child-nested")
    if scenario in PLAN_SCENARIOS:
        return exercise_plan(terminal, managed, initial, rejected=scenario == "plan-rejected")
    terminal.expect(b"Enter to submit or Escape to cancel:")
    if scenario == "cancel":
        return exercise_cancel(terminal, managed, initial)
    children = []
    if managed:
        collector = managed.collector_status()
        assert collector["healthy_runs"] == "1" and collector["gap_detected_runs"] == "0"
        assert int(collector["accepted_submissions"]) > 0
        question = managed.expect(terminal, run=initial["run"], pending="1", badge="NeedsInput")
        assert question["unverified"] == "1"
    if child_start:
        child = exercise_child(terminal, managed, initial, child_start, background=scenario == "child")
        if managed:
            children.append(child)
            if nested_start:
                children.append(exercise_child(terminal, managed, child, nested_start, background=False))
            managed.expect(terminal, run=initial["run"], pending="1", process="Running", response="false")
            snapshot = managed.snapshot()
            assert int(snapshot["agents"]) == len(children) + 1 and snapshot["requests"] == "2"
    elif managed:
        snapshot = managed.snapshot()
        assert snapshot["agents"] == "1"
        assert snapshot["requests"] == snapshot["attention"] == "2"
    if managed:
        managed.focus_question(terminal, question, offline_restart=scenario == "offline-restart")
        if scenario == "forged-output":
            exercise_forged_output(terminal, managed, question)
        if children:
            view = managed.companion.slots()
            assert set(assigned_slots(view)) == {initial["run"]}
            assert not view["ended"]
            for child in children:
                managed.reject_child_focus(child)
        managed.acknowledge_question(question)
        managed.reject_foreign_hook(question)
        if scenario == "restart":
            managed.verify_crash_history(terminal, question)
            return terminal.events()
        if fault == "malformed-result":
            before_view, before_slot = managed.presented_question(question)
            managed.companion.close()
    terminal.send(b"1\r")
    terminal.event(hook_event_name="PostToolUse", tool_name="AskUserQuestion")
    terminal.event(hook_event_name="Stop")
    if fault == "malformed-result":
        managed.verify_gap_resync(terminal, question, before_view, before_slot)
    elif managed:
        resolved = managed.expect(terminal, run=initial["run"], pending="0", resolved="1", response="true")
        assert resolved["process"] == "Running"
        assert resolved["unverified"] == "1"
    if fault == "timeout":
        wait_for_hook_timeout(terminal)
    if scenario == "resume":
        terminal.deadline = time.monotonic() + 20
        terminal.send(b"/compact\r")
        terminal.event(hook_event_name="SessionStart", source="compact", session_id=session)
        if managed:
            compacted, = managed.agents()
            assert compacted["run"] == initial["run"] and compacted["terminal"] == initial["terminal"]
            assert compacted["collection"] == "Healthy" and compacted["resolved"] == "1"
    if scenario in {"clear", "resume"}:
        terminal.send(b"/clear\r")
        cleared = terminal.event(hook_event_name="SessionStart", source="clear")
        if managed:
            current = managed.expect_replacement(
                terminal, current, collection="Healthy", session=cleared["session_id"],
            )
    if scenario == "resume":
        terminal.send(b"Corvid second fixture conversation\r")
        terminal.event(hook_event_name="Stop", session_id=cleared["session_id"])
        terminal.send(f"/resume {session}\r".encode())
        terminal.event(hook_event_name="SessionStart", source="resume", session_id=session)
        if managed:
            current = managed.expect_replacement(
                terminal, current, collection="GapDetected", session=session,
            )
            history = managed.agents()
            assert len(history) == 3 and current["run"] != initial["run"]
            old = next(run for run in history if run["run"] == initial["run"])
            assert old["resolved"] == "1" and old["end"] == "ContextReplaced"
            slot, binding = managed.focus_capture
            result = managed.companion.run(
                "focus-slot", "--slot", slot["capture"], "--binding", binding["capture"],
            )
            assert "terminal_id=" + current["terminal"] in result.stdout
    terminal.send(b"/exit\r")
    assert terminal.process.wait(timeout=5) == 0, "fixture Claude exit failed"
    if managed:
        managed.expect(terminal, run=current["run"], process="Exited")
        for child in children:
            managed.expect(terminal, run=child["run"], process="Unknown", end="ContextEnded")
    return terminal.events()


def release_foreground_children(terminal, managed, child, *, nested):
    nested_child = None
    if nested:
        offset = terminal.events().index(child) + 1
        nested_child = terminal.event(after=offset, hook_event_name="SubagentStart")
    reader = nested_child or child
    terminal.event(hook_event_name="PostToolUse", tool_name="Read", agent_id=reader["agent_id"])
    if managed:
        snapshot = managed.snapshot()
        assert snapshot["agents"] == "1" and snapshot["requests"] == "0"
    terminal.trace.with_name("child-release").touch()
    if nested:
        terminal.event(hook_event_name="PostToolUse", tool_name="Agent", agent_id=child["agent_id"])
        # The descendant's own link is confirmed, but the parent's result is still pending.
        time.sleep(5.3)
        if managed:
            snapshot = managed.snapshot()
            assert snapshot["agents"] == "1" and snapshot["requests"] == "0"
        terminal.trace.with_name("parent-release").touch()
    return nested_child


def exercise_child(terminal, managed, parent, start, *, background):
    child = None
    if managed:
        identity = {
            "parent_run": parent["run"], "actor": start["agent_id"],
            "terminal": parent["terminal"], "process": "Unknown",
        }
        if background:
            child = managed.expect(terminal, **identity, badge="Working", response="false")
    if background:
        terminal.trace.with_name("child-release").touch()
    terminal.event(hook_event_name="SubagentStop", agent_id=start["agent_id"])
    if managed:
        responded = managed.expect(terminal, **identity, badge="Unknown", response="true")
        assert "pid" not in responded
        if child:
            assert responded["run"] == child["run"]
        child = responded
    return child


def exercise_forged_output(terminal, managed, question):
    before, slot = managed.presented_question(question)
    clients = [*managed.command, "control", "clients"]
    selection = subprocess.check_output(clients, timeout=3)
    attention = managed.attention()
    snapshot = managed.snapshot()
    events = terminal.events()

    marker = managed.directory / "project/corvid-forged-command"
    command = f"$(touch {marker.name})".encode()
    invocation = next(event for event in events if event["hook_event_name"] == "PreToolUse"
                      and event.get("tool_name") == "AskUserQuestion")
    forged_result = json.dumps(invocation | {"hook_event_name": "PostToolUse"}).encode()
    write_forged_output(terminal, question["pid"], command, forged_result)

    assert not marker.exists()
    assert terminal.events() == events
    assert subprocess.check_output(clients, timeout=3) == selection
    assert managed.attention() == attention
    after_snapshot = managed.snapshot()
    for field in ("terminals", "clients", "agents", "requests", "attention"):
        assert after_snapshot[field] == snapshot[field]
    agent, = managed.agents()
    for field in ("run", "terminal", "pid", "session", "collection", "pending", "resolved", "response"):
        assert agent[field] == question[field]

    after, same_slot = managed.presented_question(question)
    assert same_slot == slot and after["bindings"] == before["bindings"]


def write_forged_output(terminal, pid, command, forged_result):
    direction_override = "\u202e".encode()
    forged_event = b'{"hook_event_name":"Stop","session_id":"forged"}'
    sequences = (
        b"\x1b]2;forged-title " + command + b"\x1b\\",
        b"\x1b]52;c;Rk9SR0VE\x07",
        b"\x1b]52;c;?\x07",
        b"\x1b]9;" + forged_result + b"\x07",
        b"\x1b]1337;SetUserVar=forged\x07",
        b"\x1b[8;7;11t",
        b"\x1b[25;1H\x1b[2KApprove " + command + direction_override + b"?",
        b"\x1b[26;1H\x1b[2K" + forged_event,
        b"\x1b[27;1H\x1b[2KCORVID-FORGED-OUTPUT-END",
    )

    terminal.screen = b""
    output = os.open(f"/proc/{pid}/fd/1", os.O_WRONLY | os.O_NOCTTY)
    try:
        size = os.get_terminal_size(output)
        for sequence in sequences:
            assert os.write(output, sequence) == len(sequence)
        terminal.expect(b"CORVID-FORGED-OUTPUT-END")
        assert os.get_terminal_size(output) == size
    finally:
        os.close(output)

    assert command + b"?" in terminal.text()
    assert forged_event in terminal.text()
    for forbidden in (b"\x1b]", b"\x1b[8;7;11t", b"forged-title", b"Rk9SR0VE", direction_override):
        assert forbidden not in terminal.screen, repr(forbidden)


def wait_for_hook_timeout(terminal):
    trace = terminal.trace.with_name(FAULT_TRACE)

    def stopped():
        entries = trace.read_text().splitlines() if trace.exists() else []
        if not entries:
            return False
        assert len(entries) == 1, "hook completed its full sleep"
        status, pid = entries[0].split()
        assert status == "started" and pid.isdecimal()
        return not Path(f"/proc/{pid}").exists()

    terminal.wait(stopped, "provider to terminate the timed-out hook")


def exercise_plan(terminal, managed, initial, *, rejected):
    terminal.expect(b"Would you like to proceed?")
    approval = None
    if managed:
        managed.expect(terminal, run=initial["run"], pending="1", unverified="1", badge="NeedsInput")
        approval = request_for(managed.command, initial["run"], "PlanApproval")
        assert approval["request_status"] == "Pending"
    if rejected:
        reject_plan(terminal, managed, initial, approval)
    else:
        terminal.send(b"1\r")
        terminal.event(hook_event_name="PostToolUse", tool_name="ExitPlanMode")
        terminal.event(hook_event_name="Stop")
        if managed:
            managed.expect(
                terminal, run=initial["run"], pending="0", resolved="1", unverified="1", process="Running",
            )
    terminal.send(b"/exit\r")
    assert terminal.process.wait(timeout=5) == 0
    return terminal.events()


def reject_plan(terminal, managed, initial, approval):
    if managed:
        managed.companion = Companion(managed.command, managed.directory)
        before = assigned_slots(managed.companion.start())[initial["run"]]
        assert before["pending"] == "1"
    terminal.expect(b"No, keep planning")
    terminal.send(b"3\r")
    terminal.expect(b"option 3 (No, keep planning)")
    terminal.screen = b""
    terminal.send(b"x")
    terminal.expect(b"x")
    terminal.send(b"\r")
    terminal.wait(terminal.trace.with_name(PLAN_REJECTION).is_file, "the model to receive the rejected plan")
    response = terminal.event(hook_event_name="Stop")
    if managed:
        managed.expect(
            terminal, run=initial["run"], process="Running", response="true",
            pending="0", unverified="2", resolved="0", cancelled="0",
        )
        current = request_for(managed.command, initial["run"], "PlanApproval")
        assert current["request_id"] == approval["request_id"]
        assert current["attention_id"] == approval["attention_id"]
        assert current["request_status"] == "Unverified"

        def approval_uncertain(view):
            slot = assigned_slots(view).get(initial["run"])
            return slot and slot["pending"] == "0" and slot["unverified"] == "2"

        view = managed.companion.wait(approval_uncertain)
        after = assigned_slots(view)[initial["run"]]
        assert after["slot"] == before["slot"] and after["revision"] == before["revision"]

    offset = len(terminal.events())
    terminal.send(b"Corvid after plan rejection\r")
    followup = terminal.event(after=offset, hook_event_name="UserPromptSubmit")
    assert followup["prompt_id"] != response["prompt_id"]
    terminal.event(hook_event_name="Stop", prompt_id=followup["prompt_id"])


def exercise_cancel(terminal, managed, initial):
    if managed:
        managed.expect(terminal, run=initial["run"], pending="1", unverified="1", badge="NeedsInput")
    offset = len(terminal.events())
    terminal.send(b"\x1b")
    time.sleep(0.5)
    terminal.send(b"Corvid after question cancellation\r")
    followup = terminal.event(after=offset, hook_event_name="UserPromptSubmit")
    terminal.event(hook_event_name="Stop", prompt_id=followup["prompt_id"])
    if managed:
        managed.expect(
            terminal, run=initial["run"], pending="0", unverified="2", cancelled="0", resolved="0",
        )
    terminal.send(b"/exit\r")
    assert terminal.process.wait(timeout=5) == 0
    return terminal.events()


def run_offline_pair(first_args, second_args, managed, trace):
    with open_terminal(first_args, managed.environment, managed.project, trace) as first:
        first_session = initialize_terminal(first)
        offset = len(first.events())
        with open_terminal(second_args, managed.environment, managed.project, trace) as second:
            second_session = second.event(after=offset, hook_event_name="SessionStart", source="startup")["session_id"]
            first_run = managed.expect(first, session=first_session, process="Running", collection="Healthy")
            second_run = managed.expect(second, session=second_session, process="Running", collection="Healthy")
            for field in ("session", "run", "terminal", "pid"):
                assert first_run[field] != second_run[field]
            assert managed.collector_status()["healthy_runs"] == "2"
            collect_offline_questions(managed, ((first, first_run), (second, second_run)))
            for terminal in (first, second):
                terminal.send(b"/exit\r")
                assert terminal.process.wait(timeout=5) == 0
            return first.events()


def run_snapshot_race(agent_args, managed, trace, layout):
    agent_pane, shell_pane, destination = layout
    attach = [*managed.command, "attach"]
    shell_args = [*attach, "--workspace", shell_pane.workspace, "--tab", shell_pane.tab]
    shell_command = 'stty -echo; printf "A8-MOVER-READY\\n"; IFS= read -r line; [ "$line" = q ]'
    with open_terminal(
        [*shell_args, "--", "/bin/sh", "-c", shell_command],
        managed.environment, managed.project, trace,
    ) as shell:
        shell.expect(b"A8-MOVER-READY")
        shell.send(b"\x1dd")
        assert shell.process.wait(timeout=5) == 0

    prompt = b"Enter to submit or Escape to cancel:"
    with open_terminal(agent_args, managed.environment, managed.project, trace) as terminal:
        session = initialize_terminal(terminal)
        initial = managed.expect(terminal, session=session, collection="Healthy", process="Running")
        with paused_watch(managed.command, managed.directory) as watch:
            terminal.send(b"Corvid hook schema fixture\r")
            terminal.expect(prompt)
            managed.expect(terminal, run=initial["run"], pending="1", unverified="1")
            question = request_for(managed.command, initial["run"])
            assert question["request_status"] == "Pending"
            terminal.send(b"\x1dd")
            assert terminal.process.wait(timeout=5) == 0

            move_args = [
                *shell_args, "--claim-workspace", destination,
                "--move-tab", agent_pane.tab, "--to-workspace", destination,
            ]
            with open_terminal(move_args, managed.environment, managed.project, trace) as mover:
                mover.expect(b"A8-MOVER-READY")
                mover.wait(
                    lambda: tab_in_destination(managed.command, destination, agent_pane.tab),
                    "the agent tab to move",
                )
                expected = managed.snapshot()
                assert expected["requests"] == expected["attention"] == "2"
                assert expected["terminals"] == "2" and expected["clients"] == expected["agents"] == "1"
                watch.finish(agent_pane, destination, initial, question, expected)
                mover.send(b"q\r")
                assert mover.process.wait(timeout=5) == 0

    reopen = [*attach, "--workspace", destination, "--tab", agent_pane.tab]
    with open_terminal(reopen, managed.environment, managed.project, trace) as terminal:
        terminal.expect(prompt)
        managed.expect(
            terminal, run=initial["run"], terminal=initial["terminal"], pid=initial["pid"], pending="1",
        )
        assert request_for(managed.command, initial["run"]) == question
        answer_offline_question(terminal, initial, question, managed)
        terminal.send(b"/exit\r")
        assert terminal.process.wait(timeout=5) == 0
        return terminal.events()


def tab_in_destination(command, destination, tab):
    layout = subprocess.check_output([*command, "control", "workspaces"], text=True, timeout=3)
    tail = layout.partition(f"workspace {destination} Destination\n")[2]
    return f"  tab {tab} Agent\n" in tail


def collect_offline_questions(managed, agents):
    managed.companion = companion = Companion(managed.command, managed.directory)
    companion.start()
    runs = {agent["run"] for _, agent in agents}
    before = companion.wait(lambda view: runs == set(assigned_slots(view)))
    assert all(slot["pending"] == "0" for slot in assigned_slots(before).values())
    companion.close()
    assert companion.slots() is None

    questions = []
    for (terminal, agent), prompt in zip(agents, OFFLINE_PROMPTS, strict=True):
        terminal.send(prompt + b"\r")
        terminal.expect(b"Enter to submit or Escape to cancel:")
        managed.expect(terminal, run=agent["run"], pending="1", unverified="1", collection="Healthy")
        question = request_for(managed.command, agent["run"])
        assert question["request_status"] == "Pending"
        questions.append(question)
    first_question, second_question = questions
    assert first_question["request_id"] != second_question["request_id"]
    assert companion.slots() is None

    trace = agents[0][0].events()
    after = companion.start()
    assert after["header"]["remote_epoch"] == before["header"]["remote_epoch"]
    assert after["header"]["control_capture"] != before["header"]["control_capture"]
    for run, slot in assigned_slots(before).items():
        restored = assigned_slots(after)[run]
        assert (restored["slot"], restored["revision"]) == (slot["slot"], slot["revision"])
        assert restored["pending"] == "1" and restored["last_badge"] == "NeedsInput"
    assert agents[0][0].events() == trace
    assert [request_for(managed.command, question["run_id"]) for question in questions] == questions

    (first, first_agent), (second, second_agent) = agents
    answer_offline_question(first, first_agent, first_question, managed)
    assert request_for(managed.command, second_agent["run"]) == second_question
    second.expect(b"Enter to submit or Escape to cancel:")
    answer_offline_question(second, second_agent, second_question, managed)


def answer_offline_question(terminal, agent, question, managed):
    terminal.send(b"1\r")
    terminal.event(hook_event_name="PostToolUse", session_id=agent["session"], tool_name="AskUserQuestion")
    managed.expect(terminal, run=agent["run"], pending="0", resolved="1", response="true", unverified="1")
    resolved = request_for(managed.command, agent["run"])
    assert resolved["request_id"] == question["request_id"] and resolved["request_status"] == "Resolved"
    permission = request_for(managed.command, agent["run"], "Permission")
    assert permission["request_status"] == "Unverified"


class CorvidFixture:
    def __init__(self, binary, claude, directory, project, environment):
        self.command = [binary, "--runtime-dir", directory / "run"]
        self.directory = directory
        self.companion = None
        self.focus_capture = None
        environment["PATH"] = f"{claude.parent}:{environment['PATH']}"
        environment["TERMINFO"] = str(directory / "terminfo")
        profile = Path(__file__).resolve().parents[3] / "spikes/terminal/corvid.terminfo"
        subprocess.run(["tic", "-x", "-o", environment["TERMINFO"], profile], check=True, timeout=3)
        self.environment = environment
        self.project = project
        self.start_daemon()

    def start_daemon(self):
        self.process = subprocess.Popen(
            [*self.command, "daemon", "--state-dir", self.directory / "state"],
            cwd=self.project, env=self.environment,
        )
        try:
            wait_for_daemon(self.command, self.process)
        except BaseException:
            stop(self.process)
            raise

    def agents(self):
        output = subprocess.check_output([*self.command, "control", "agents"], text=True, timeout=3)
        agents = []
        for line in output.splitlines():
            run, *fields = line.split()
            agent = dict(field.split("=", 1) for field in fields)
            agent["run"] = run
            agents.append(agent)
        return agents

    def snapshot(self):
        output = subprocess.check_output([*self.command, "control", "snapshot"], text=True, timeout=3)
        return dict(field.split("=", 1) for field in output.split())

    def collector_status(self):
        output = subprocess.check_output([*self.command, "control", "collector-status"], text=True, timeout=3)
        return fields(output)

    def verify_crash_history(self, terminal, question):
        def saved():
            status = subprocess.check_output([*self.command, "control", "status"], text=True, timeout=3)
            return fields(status).get("persistence") == "saved"

        terminal.wait(saved, "the pending question and acknowledgment to be saved")
        before = self.snapshot()
        archived_file = self.directory / "state/state.json"
        content = archived_file.read_text()
        assert "Which fixture option do you choose?" not in content
        assert "Corvid hook schema fixture" not in content
        saved_run, = json.loads(content)["history"]
        assert saved_run["run"] == question["run"] and saved_run["session"] == question["session"]
        pending, = [item for item in saved_run["requests"] if item["status"] == "pending"]
        ack, = [item for item in saved_run["attention"] if item["acknowledged_unix_ms"] is not None]
        assert ack["request_id"] == pending["id"]

        self.process.kill()
        self.process.wait(timeout=5)
        assert terminal.process.wait(timeout=5) != 0
        self.start_daemon()
        after = self.snapshot()
        assert after["host_id"] == before["host_id"] and after["server_epoch"] != before["server_epoch"]
        assert after["sequence"] == after["agents"] == after["requests"] == after["terminals"] == after["clients"] == "0"
        assert self.agents() == [] and self.attention() == []
        collector = self.collector_status()
        assert all(value == "0" for key, value in collector.items() if key.endswith("_runs")), collector
        assert collector["accepted_submissions"] == collector["rejected_submissions"] == "0", collector

        output = subprocess.check_output([*self.command, "control", "history"], text=True, timeout=3)
        run, *items = map(fields, output.splitlines())
        assert run["epoch"] == before["server_epoch"] and run["run"] == question["run"]
        assert run["state"] == "Historical" and run["process"] == "Unknown"
        assert run["last_process"] == "Running" and run["terminal"] == question["terminal"]
        request, = [item for item in items if item.get("request") == pending["id"]]
        assert request["kind"] == "Question" and request["status"] == "Unverified"
        alert, = [item for item in items if item.get("attention") == ack["id"]]
        assert int(alert["acknowledged_unix_ms"]) == ack["acknowledged_unix_ms"]

        old_slot, old_binding = self.focus_capture
        focus = subprocess.run(
            [*self.command, "controller", "--controller-id", self.companion.controller,
             "--host-id", before["host_id"], "--server-epoch", before["server_epoch"],
             "focus", "--client-id", old_binding["client_id"], "--binding-revision", old_binding["revision"],
             "--run-id", question["run"]],
            capture_output=True, text=True, timeout=3,
        )
        assert focus.returncode != 0 and "EpochMismatch" in focus.stderr
        view = self.companion.wait(lambda view: view["header"].get("remote_epoch") == after["server_epoch"])
        slot, = [slot for slot in view["slots"] if slot.get("run") == old_slot["run"]]
        assert slot["availability"] == "Historical" and not view["bindings"]

    def presented_question(self, question):
        def has_question(view):
            return view["bindings"] and any(
                slot.get("run", "").endswith("/" + question["run"]) and slot["pending"] == "1"
                for slot in view["slots"]
            )

        view = self.companion.wait(has_question)
        slot = next(slot for slot in view["slots"] if slot.get("run", "").endswith("/" + question["run"]))
        return view, slot

    def verify_gap_resync(self, terminal, question, before_view, before_slot):
        self.expect(
            terminal, run=question["run"], collection="GapDetected", badge="Unknown",
            process="Running", pending="0", resolved="0", unverified="2",
        )
        collector = self.collector_status()
        assert collector["healthy_runs"] == "0" and collector["gap_detected_runs"] == "1"
        assert int(collector["accepted_submissions"]) > 0
        view = self.companion.start()
        slot = next(item for item in view["slots"] if item.get("run") == before_slot["run"])
        assert slot["slot"] == before_slot["slot"] and slot["revision"] == before_slot["revision"]
        assert slot["last_badge"] == "Unknown" and slot["collection"] == "GapDetected"
        assert slot["pending"] == "0" and slot["unverified"] == "2"
        assert view["header"]["remote_epoch"] == before_view["header"]["remote_epoch"]
        assert view["header"]["control_capture"] != before_view["header"]["control_capture"]

    def question_is_parked(self, question):
        clients = subprocess.check_output(
            [*self.command, "control", "clients"], text=True, timeout=3,
        ).splitlines()
        return (len(clients) == 1 and "terminal=" in clients[0]
                and "terminal=" + question["terminal"] not in clients[0])

    def park_question(self, tty, question):
        self.companion = Companion(self.command, self.directory)
        self.companion.start()
        tty.screen = b""
        tty.send(b"\x1dr")
        tty.expect(b"Controller pairing code")
        code = re.search(rb"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", tty.screen)[0].decode()
        self.companion.run("pair", input=code + "\n")
        tty.screen = b""
        tty.send(b"\r")
        tty.expect(b"Enter to submit or Escape to cancel:")
        tty.send(b"\x1dt")
        tty.wait(lambda: self.question_is_parked(question), "second terminal selected")
        assert self.snapshot()["terminals"] == "2"

    def focus_question(self, tty, question, *, offline_restart=False):
        self.park_question(tty, question)
        old_view, old_slot = self.presented_question(question)
        old_binding = old_view["bindings"][0]
        self.companion.close()
        if offline_restart:
            self.restart_with_control_unavailable(old_view, old_slot, old_binding)
        else:
            self.companion.start()
        view, slot = self.presented_question(question)
        binding = view["bindings"][0]
        assert slot["slot"] == old_slot["slot"] and slot["revision"] == old_slot["revision"]
        assert binding["client_id"] == old_binding["client_id"] and binding["revision"] == old_binding["revision"]
        assert view["header"]["controller_id"] == self.companion.controller
        stale = self.companion.run(
            "focus-slot", "--slot", old_slot["capture"], "--binding", old_binding["capture"], check=False,
        )
        assert stale.returncode != 0 and "StaleBinding" in stale.stderr
        assert self.question_is_parked(question)

        tty.screen = b""
        applied = self.companion.run("focus-slot", "--slot", slot["capture"], "--binding", binding["capture"]).stdout
        assert "terminal_id=" + question["terminal"] in applied
        assert "local_activation=ManualRequired" in applied
        self.focus_capture = (slot, binding)
        tty.expect(b"Enter to submit or Escape to cancel:")

    def restart_with_control_unavailable(self, before, slot, binding):
        saved = (self.directory / "companion/slots.json").read_text(encoding="utf-8")
        for live_value in (
            binding["client_id"], before["header"]["control_capture"].split("/", 1)[1],
            slot["latest_attention_id"], "Corvid hook schema fixture", "Which fixture option do you choose?",
        ):
            assert live_value not in saved

        control = self.directory / "run/control.sock"
        blocked = control.with_name("control-offline.sock")
        control.rename(blocked)
        try:
            offline = self.companion.start("Offline")
            epoch = offline["header"]["companion_epoch"]
            assert epoch != before["header"]["companion_epoch"]
            assert not offline["bindings"] and "control_capture" not in offline["header"]
            run = slot["run"].rsplit("/", 1)[-1]
            cached = assigned_slots(offline)[run]
            assert cached["availability"] == "Offline"
            assert cached["last_badge"] == "NeedsInput" and cached["pending"] == "1"
            assert int(cached["last_observed_unix_ms"]) > 0
            for name in (
                "slot", "revision", "run", "last_badge", "collection", "evidence",
                "pending", "unverified", "unseen", "last_observed_unix_ms",
            ):
                assert cached[name] == slot[name], name
            assert "latest_attention_id" not in cached

            binding_capture = f"{epoch}/{binding['capture'].split('/', 1)[1]}"
            focus = self.companion.run(
                "focus-slot", "--slot", cached["capture"], "--binding", binding_capture, check=False,
            )
            assert focus.returncode != 0 and "ConnectionRequired" in focus.stderr, focus.stderr
            control_capture = f"{epoch}/{before['header']['control_capture'].split('/', 1)[1]}"
            ack = self.companion.run(
                "ack", "--control", control_capture, "--run", cached["run"],
                "--attention-id", slot["latest_attention_id"], check=False,
            )
            assert ack.returncode != 0 and "ConnectionRequired" in ack.stderr, ack.stderr
        finally:
            blocked.rename(control)
        self.companion.wait(lambda view: view["header"]["connection"] == "Ready")

    def expect_replacement(self, terminal, previous, **expected):
        self.expect(terminal, run=previous["run"], end="ContextReplaced", process="Running")
        current = self.expect(terminal, end="Unspecified", process="Running", **expected)
        assert current["terminal"] == previous["terminal"] and current["pid"] == previous["pid"]
        assert current["run"] != previous["run"]
        for field in ("resolved", "pending", "unverified", "cancelled"):
            assert current[field] == "0"
        self.verify_reassigned_slot(previous["run"], current["run"])
        return current

    def verify_reassigned_slot(self, previous, replacement):
        slot, binding = self.focus_capture

        def replaced(view):
            return any(item.get("run", "").endswith("/" + replacement) for item in view["slots"])

        view = self.companion.wait(replaced)
        current = next(item for item in view["slots"] if item.get("run", "").endswith("/" + replacement))
        assert current["slot"] == slot["slot"] and int(current["revision"]) > int(slot["revision"])
        assert any(run.endswith("/" + previous) for run in view["ended"])
        assert binding["capture"] == view["bindings"][0]["capture"]
        stale = self.companion.run(
            "focus-slot", "--slot", slot["capture"], "--binding", binding["capture"], check=False,
        )
        assert stale.returncode != 0 and "StaleBinding" in stale.stderr
        self.focus_capture = (current, binding)

    def acknowledge_question(self, question):
        view, slot = self.presented_question(question)
        first = next(alert for alert in self.attention()
                     if alert["run_id"] == question["run"] and alert["request_kind"] == "Question")
        assert first["acknowledged"] == "false"
        ack = ["ack", "--control", view["header"]["control_capture"], "--run", slot["run"],
               "--attention-id", first["attention_id"]]
        result = fields(self.companion.run(*ack).stdout)
        repeated = fields(self.companion.run(*ack).stdout)
        assert result["attention_id"] == repeated["attention_id"] == first["attention_id"]
        assert result["acknowledged_unix_ms"] == repeated["acknowledged_unix_ms"]
        actual = next(agent for agent in self.agents() if agent["run"] == question["run"])
        assert actual["pending"] == "1" and actual["unverified"] == "1"
        output = subprocess.check_output([*self.command, "control", "attention"], text=True, timeout=3)
        assert output.count("acknowledged=true") == 1

    def attention(self):
        return attention(self.command)

    def expect(self, tty, **fields):
        def matching():
            return next((agent for agent in self.agents()
                         if all(agent.get(key) == value for key, value in fields.items())), None)

        tty.wait(matching, str(fields))
        return matching()

    def reject_foreign_hook(self, question):
        child_env = Path(f"/proc/{question['pid']}/environ").read_bytes()
        required = {b"CORVID_LAUNCH_ASSOCIATION", b"CORVID_HOST_ID", b"CORVID_SERVER_EPOCH"}
        scope = {}
        for entry in child_env.split(b"\0"):
            key, _, value = entry.partition(b"=")
            if key in required:
                scope[key.decode()] = value.decode()
        assert len(scope) == len(required)

        payload = {
            "hook_event_name": "PreToolUse",
            "session_id": question["session"],
            "prompt_id": "unowned-prompt",
            "tool_name": "AskUserQuestion",
            "tool_use_id": "unowned-tool",
        }
        result = subprocess.run(
            [*self.command, "observe", "--provider", "claude"],
            input=json.dumps(payload), env=scope, capture_output=True, text=True, timeout=1,
        )
        assert result.returncode == 0 and not result.stdout and not result.stderr
        actual = next(agent for agent in self.agents() if agent["run"] == question["run"])
        assert actual["pending"] == question["pending"]

    def reject_child_focus(self, child):
        slot, binding = self.focus_capture
        host, epoch, _ = slot["run"].split("/")
        clients = [*self.command, "control", "clients"]
        before = subprocess.check_output(clients, timeout=3)
        refused = subprocess.run([
            *self.command, "controller", "--controller-id", self.companion.controller,
            "--host-id", host, "--server-epoch", epoch, "focus",
            "--client-id", binding["client_id"], "--binding-revision", binding["revision"],
            "--run-id", child["run"],
        ], capture_output=True, text=True, timeout=3)
        assert refused.returncode != 0 and "TargetGone" in refused.stderr, refused.stderr
        assert subprocess.check_output(clients, timeout=3) == before


def verify_observer_crashes(managed, scenario, events, configured):
    optional = set()
    if scenario not in CHILD_SCENARIOS:
        observed = {event["hook_event_name"] for event in events}
        optional.update({"SubagentStart", "SubagentStop"} - observed)
    if scenario in {"cancel", "plan-rejected"}:
        optional.add("PostToolUse")
    if scenario == "cancel":
        optional.add("PostToolBatch")

    verify_crashed_hooks(managed.directory / FAULT_TRACE, configured - optional, optional=optional)
    collector = managed.collector_status()
    assert collector["accepted_submissions"] == collector["rejected_submissions"] == "0"


def capture(binary, directory, port, scenario, corvid, fault):
    trace, environment, crash_hooks = configure(directory, port, corvid, fault)
    project = directory / "project"
    project.mkdir()
    if scenario in CHILD_SCENARIOS:
        (project / "child-note.txt").write_text("Synthetic fixture note.\n")
        environment["API_TIMEOUT_MS"] = "9000"
    if scenario in SYNC_CHILD_SCENARIOS:
        environment["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] = "1"
    if scenario == "child-nested":
        environment["CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH"] = "2"
    arguments = [
        "--model", "sonnet", "--strict-mcp-config", "--no-chrome", "--ax-screen-reader",
        "--permission-mode", "plan" if scenario in PLAN_SCENARIOS else "manual",
    ]
    if corvid is None:
        args = ["setsid", "--ctty", "--wait", str(binary.resolve()), *arguments]
        return run_terminal(args, environment, project, trace, scenario, None, fault)

    race_layout = seed_history(directory) if scenario == "snapshot-race" else None
    managed = CorvidFixture(corvid, binary, directory, project, environment)
    try:
        if fault == "missing-collector":
            (directory / "run/collector.sock").unlink()
        args = [*managed.command, "attach", "--agent", "claude", "--", *arguments]
        if race_layout is not None:
            return run_snapshot_race(args, managed, trace, race_layout)
        if scenario == "offline-pair":
            other = [*managed.command, "attach", "--new-workspace", "Other", "--agent", "claude", "--", *arguments]
            return run_offline_pair(args, other, managed, trace)
        active_collector = managed if fault in (None, "malformed-result") else None
        events = run_terminal(args, environment, project, trace, scenario, active_collector, fault)
        if fault is not None:
            agent, = managed.agents()
            assert agent["process"] == "Exited"
            if fault == "timeout":
                assert agent["collection"] == "Healthy" and agent["response"] == "false"
                assert agent["resolved"] == "1"
            elif fault == "malformed-result":
                assert agent["collection"] == "GapDetected" and agent["resolved"] == "0"
                assert (directory / FAULT_TRACE).read_text().splitlines() == ["invalid tool-use ID"]
            else:
                assert agent["collection"] == "NotRegistered"
                if fault == "helper-crash":
                    verify_observer_crashes(managed, scenario, events, crash_hooks)
                if fault == "collector-saturated":
                    # The batch observer is called once per tool batch, including Write and ToolSearch.
                    attempts = 10 if scenario == "plan" else 8
                    assert (directory / FAULT_TRACE).read_text().splitlines() == [SATURATION] * attempts
                    collector = managed.collector_status()
                    assert int(collector["saturated_connections"]) >= attempts
                    assert collector["healthy_runs"] == collector["gap_detected_runs"] == "0"
        return events
    finally:
        if managed.companion is not None:
            managed.companion.close()
        stop(managed.process)


def run_terminal(args, environment, project, trace, scenario, managed, fault):
    with open_terminal(args, environment, project, trace) as terminal:
        return exercise(terminal, scenario, managed, fault)


@contextmanager
def open_terminal(args, environment, project, trace):
    master, slave = pty.openpty()
    try:
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))
        process = subprocess.Popen(
            args, env=environment, cwd=project, stdin=slave, stdout=slave, stderr=slave,
        )
        try:
            yield FixtureTerminal(process, master, trace)
        finally:
            stop(process)
    finally:
        os.close(master)
        os.close(slave)


def sanitize(events):
    ids = {}
    for event in events:
        for record in (event, *event.get("tool_calls", [])):
            for field in IDS:
                if field not in record:
                    continue
                value = record[field]
                if value not in ids:
                    index = len(ids) + 1
                    if field == "tool_use_id":
                        ids[value] = f"toolu_fixture_{index}"
                    else:
                        ids[value] = f"00000000-0000-4000-8000-{index:012d}"
                record[field] = ids[value]
        print(json.dumps(event, separators=(",", ":")))


def verify(events, scenario):
    if scenario == "offline-pair":
        questions = [event for event in events if event["hook_event_name"] == "PreToolUse"
                     and event.get("tool_name") == "AskUserQuestion"]
        assert len(questions) == 2 and questions[0]["session_id"] != questions[1]["session_id"]
        assert questions[0]["tool_use_id"] == questions[1]["tool_use_id"]
        return
    first = events[0]
    assert first["hook_event_name"] == "SessionStart"
    assert first["source"] == "startup"
    assert "prompt_id" not in first
    session = first["session_id"]
    submitted = next(event for event in events if event["hook_event_name"] == "UserPromptSubmit")
    turn = submitted["prompt_id"]
    tool = "ExitPlanMode" if scenario in PLAN_SCENARIOS else "AskUserQuestion"
    before = next(event for event in events if event["hook_event_name"] == "PreToolUse"
                  and event.get("tool_name") == tool)
    permission = next(event for event in events if event["hook_event_name"] == "PermissionRequest"
                      and event.get("tool_name") == tool)
    for event in (submitted, before, permission):
        assert event["session_id"] == session
        assert event["prompt_id"] == turn
    assert "tool_use_id" not in permission
    outcomes = [event for event in events if event.get("tool_name") == tool
                and event["hook_event_name"] in {"PostToolUse", "PostToolUseFailure", "PermissionDenied"}]
    if scenario == "restart":
        assert not outcomes and not any(event["hook_event_name"] == "Stop" for event in events)
        return
    response = next(event for event in events if event["hook_event_name"] == "Stop")
    assert response["session_id"] == session
    if scenario in PLAN_SCENARIOS:
        batch, = [event for event in events if event["hook_event_name"] == "PostToolBatch"]
        assert batch["session_id"] == session and batch["prompt_id"] == response["prompt_id"] == turn
        assert batch["tool_calls"] == [{"tool_name": tool, "tool_use_id": before["tool_use_id"]}]
        assert events.index(before) < events.index(batch) < events.index(response)
    if scenario == "cancel":
        assert not outcomes
        assert not any(event["hook_event_name"] == "PostToolBatch" for event in events)
        followup = next(event for event in events if event["hook_event_name"] == "UserPromptSubmit"
                        and event["prompt_id"] != turn)
        assert response["prompt_id"] == followup["prompt_id"] and followup["session_id"] == session
    elif scenario == "plan-rejected":
        assert not outcomes
    else:
        after, = outcomes
        assert after["hook_event_name"] == "PostToolUse"
        assert after["session_id"] == session and after["prompt_id"] == response["prompt_id"] == turn
        assert before["tool_use_id"] == after["tool_use_id"]
        if scenario == "plan":
            assert events.index(after) < events.index(batch)
    assert events[-1]["hook_event_name"] == "SessionEnd"
    assert events[-1]["reason"] == "prompt_input_exit"
    if scenario in CHILD_SCENARIOS:
        verify_children(events, scenario, session, turn, before, after)
    if scenario in {"clear", "resume"}:
        ended = next(event for event in events if event.get("reason") == "clear")
        started = next(event for event in events if event.get("source") == "clear")
        assert ended["hook_event_name"] == "SessionEnd" and ended["session_id"] == session
        assert started["hook_event_name"] == "SessionStart" and started["session_id"] != session
        if scenario == "resume":
            compact = next(event for event in events if event.get("source") == "compact")
            switching = next(event for event in events if event.get("reason") == "resume")
            resumed = next(event for event in events if event.get("source") == "resume")
            assert compact["session_id"] == resumed["session_id"] == session
            assert switching["session_id"] == started["session_id"]
            assert switching["prompt_id"] == resumed["prompt_id"]
            boundaries = [events.index(event) for event in (compact, started, switching, resumed)]
            assert boundaries == sorted(boundaries)
            assert events[-1]["session_id"] == session
        else:
            assert events[-1]["session_id"] == started["session_id"]
    else:
        assert events[-1]["session_id"] == session


def verify_children(events, scenario, session, turn, before, after):
    starts = [event for event in events if event["hook_event_name"] == "SubagentStart"]
    assert len(starts) == (2 if scenario == "child-nested" else 1)
    read, = [event for event in events if event["hook_event_name"] == "PostToolUse"
             and event.get("tool_name") == "Read"]
    assert read["agent_id"] == starts[-1]["agent_id"]
    assert read["session_id"] == session and read["prompt_id"] == turn

    parent = None
    tool_ids = []
    for child in starts:
        stopped, = [event for event in events if event["hook_event_name"] == "SubagentStop"
                    and event["agent_id"] == child["agent_id"]]
        invoked, linked = [event for event in events if event.get("tool_name") == "Agent"
                           and event.get("agent_id") == parent]
        assert invoked["hook_event_name"] == "PreToolUse" and linked["hook_event_name"] == "PostToolUse"
        assert stopped["stop_hook_active"] is False and linked["result_agent_id"] == child["agent_id"]
        assert invoked["tool_use_id"] == linked["tool_use_id"]
        tool_ids.append(invoked["tool_use_id"])
        for event in (invoked, child, stopped, linked):
            assert event["session_id"] == session and event["prompt_id"] == turn

        if scenario == "child":
            assert linked["result_status"] == "async_launched"
            order = (invoked, child, linked, before, stopped, after)
        else:
            assert linked["result_status"] == "completed"
            order = (invoked, child, read, stopped, linked, before)
        positions = [events.index(event) for event in order]
        assert positions == sorted(positions)
        parent = child["agent_id"]
    if scenario == "child-nested":
        assert tool_ids[0] == tool_ids[1], "the fixture reuses tool IDs across distinct actors"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", choices=(
        "answer", "clear", "resume", "cancel", "plan", "child", "forged-output", "restart",
        "offline-pair", "offline-restart", "snapshot-race", "plan-rejected", "child-sync", "child-nested",
    ), default="answer")
    parser.add_argument("--corvid", type=Path, help="also exercise the real Corvid daemon and collector")
    parser.add_argument("--fault", choices=(
        "missing-helper", "helper-crash", "missing-collector", "collector-saturated", "timeout",
        "malformed-result",
    ))
    args = parser.parse_args()
    if args.fault is not None and args.corvid is None:
        parser.error("--fault requires --corvid")
    healthy_scenarios = {
        "forged-output", "restart", "offline-pair", "offline-restart", "snapshot-race",
    }
    if args.scenario in healthy_scenarios and (args.corvid is None or args.fault is not None):
        parser.error("this scenario requires --corvid and a healthy collector")
    if args.fault == "collector-saturated" and args.scenario not in {"answer", "plan"}:
        parser.error("collector saturation requires --scenario answer or plan")
    if args.fault in {"timeout", "malformed-result"} and args.scenario != "answer":
        parser.error("this collector fault requires --scenario answer")
    executable = shutil.which("claude")
    if executable is None:
        parser.error("claude must be installed on PATH")
    binary = Path(executable).absolute()
    version = subprocess.check_output([binary, "--version"], text=True, timeout=3).strip()
    assert version == VERSION, f"expected {VERSION}, found {version}"
    corvid = args.corvid.resolve() if args.corvid is not None else None
    parent = Path(__file__).resolve().parents[3] if corvid is not None else None
    with tempfile.TemporaryDirectory(prefix=".corvid-claude-", dir=parent) as directory:
        server_type = ThreadingHTTPServer if args.scenario in CHILD_SCENARIOS else HTTPServer
        with server_type(("127.0.0.1", 0), ModelFixture) as server:
            server.fixture_tool = "ExitPlanMode" if args.scenario in PLAN_SCENARIOS else "AskUserQuestion"
            server.tool_sent = False
            server.plan_drafted = False
            server.plan_search_sent = False
            server.plan_directory = Path(directory) / "home/.claude/plans"
            server.plan_rejection = Path(directory) / PLAN_REJECTION
            server.child_fixture = args.scenario in CHILD_SCENARIOS
            server.nested_child = args.scenario == "child-nested"
            server.offline_pair = args.scenario == "offline-pair"
            server.questions_sent = set()
            server.agent_sent = False
            server.nested_agent_sent = False
            server.child_read_sent = threading.Event()
            server.parent_question_sent = False
            server.child_note = Path(directory) / "project/child-note.txt"
            server.child_release = Path(directory) / "child-release"
            server.parent_release = Path(directory) / "parent-release"
            server.message_count = 0
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                events = capture(
                    binary, Path(directory), server.server_port, args.scenario, corvid, args.fault,
                )
            finally:
                server.shutdown()
                thread.join()
    verify(events, args.scenario)
    sanitize(events)


if __name__ == "__main__":
    main()
