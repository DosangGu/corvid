"""Qualify installed Codex CLI 0.167.0 in a real PTY using a loopback model."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

from companion_fixture import Companion, fields
from interactive import stop, wait_for_daemon

VERSION = "codex-cli 0.167.0"
ROOT = Path(__file__).resolve().parents[3]
EVENTS = (
    "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse",
    "PermissionRequest", "Stop", "Interrupt", "PreCompact", "PostCompact",
)
CAPTURE = '''
import json
import os
from pathlib import Path
import sys

if len(sys.argv) == 3:
    payload = json.loads(sys.argv[2])
    allow = ("type", "thread-id", "turn-id")
else:
    payload = json.load(sys.stdin)
    allow = (
        "hook_event_name", "session_id", "source", "turn_id", "tool_name",
        "tool_use_id", "permission_mode", "stop_hook_active", "reason", "trigger",
    )
event = {key: payload[key] for key in allow if key in payload}
parent = Path(f"/proc/{os.getppid()}")
event["parent"] = Path(os.readlink(parent / "exe")).name
argv = (parent / "cmdline").read_bytes().rstrip(b"\\0").split(b"\\0")
if argv[1:4] == [b"app-server", b"--listen", b"unix://"] and argv[-1] == b"--managed-daemon":
    event["parent_role"] = "managed_app_server"
with open(sys.argv[1], "a") as trace:
    trace.write(json.dumps(event, separators=(",", ":")) + "\\n")
if event.get("hook_event_name") == "Stop":
    print("{}")
'''

SATURATE = '''
import os
from pathlib import Path
import socket
import sys

sockets = []
path = str(Path(os.environ["CORVID_RUNTIME_DIR"]) / "collector.sock")
for _ in range(32):
    peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    peer.settimeout(0.03)
    peer.connect(path)
    assert peer.recv(4, socket.MSG_WAITALL) == b"CRV1"
    os.set_inheritable(peer.fileno(), True)
    sockets.append(peer)
with open(sys.argv[1], "a") as trace:
    trace.write("32 handshakes\\n")
os.execv(sys.argv[2], sys.argv[2:])
'''


class ModelFixture(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers["Content-Length"])
        assert self.path == "/v1/responses" and 0 < length <= 4 * 1024 * 1024
        body = json.loads(self.rfile.read(length))
        request = next(self.server.sequence)
        item = self.output_item(body, request)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        usage = {
            "input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 15,
        }
        response = {"id": f"resp_fixture_{request}", "status": "completed", "output": [item], "usage": usage}
        events = [
            ("response.created", {"response": {"id": response["id"], "output": []}}),
            ("response.output_item.added", {"output_index": 0, "item": dict(item, status="in_progress")}),
            ("response.output_item.done", {"output_index": 0, "item": item}),
            ("response.completed", {"response": response}),
        ]
        try:
            for name, fields in events:
                event = json.dumps(dict(fields, type=name))
                self.wfile.write(f"event: {name}\ndata: {event}\n\n".encode())
        except (BrokenPipeError, ConnectionResetError):
            # The deliberate interruption can close the held response stream.
            pass

    def output_item(self, body, request):
        names = {tool.get("name") for tool in body.get("tools", [])}
        messages = body.get("input", [])
        prompt = next((str(item.get("content")) for item in reversed(messages)
                       if item.get("role") == "user"), "")
        if "exec_command" in names and "fixture approve" in prompt:
            call = self.tool_call(messages, "approval", "exec_command", {
                "cmd": "printf CORVID_TOOL_RESULT",
                "sandbox_permissions": "require_escalated",
                "justification": "May this isolated fixture print text?",
            })
            if call:
                return call
        if "request_user_input" in names:
            for name in ("answer", "cancel"):
                if f"fixture {name}" not in prompt:
                    continue
                call = self.tool_call(messages, name, "request_user_input", {"questions": [{
                    "header": "Fixture", "id": "option", "question": f"Which synthetic {name} option?",
                    "options": [
                        {"label": "Alpha (Recommended)", "description": "First fixture option"},
                        {"label": "Beta", "description": "Second fixture option"},
                    ],
                }]})
                if call:
                    return call
        return {
            "id": f"msg_fixture_{request}", "type": "message", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "Synthetic response fixture", "annotations": []}],
        }

    @staticmethod
    def tool_call(messages, label, name, arguments):
        call_id = f"call_{label}"
        if any(item.get("call_id") == call_id for item in messages):
            return None
        return {
            "id": f"fc_{label}", "type": "function_call", "call_id": call_id, "name": name,
            "status": "completed", "arguments": json.dumps(arguments),
        }


def configure(directory, project, port, binary, corvid, fault):
    home = directory / "home"
    config = home / ".codex"
    config.mkdir(parents=True, mode=0o700)
    capture = directory / "capture.py"
    trace = directory / "trace.jsonl"
    capture.write_text(CAPTURE)
    observer = [sys.executable, str(capture), str(trace)]
    handler = {"type": "command", "command": "exec " + shlex.join(observer), "timeout": 1}
    hooks = {event: [{"hooks": [handler]}] for event in EVENTS}
    if corvid is not None:
        saturate = directory / "saturate.py"
        if fault == "collector-saturated":
            saturate.write_text(SATURATE)
        generated = json.loads(subprocess.check_output([corvid, "hooks", "codex"], timeout=3))
        for event, groups in generated["hooks"].items():
            for group in groups:
                for hook in group["hooks"]:
                    if fault == "missing-helper":
                        hook["command"] = "exec " + shlex.quote(str(directory / "missing-observer"))
                    elif fault == "timeout" and event == "Stop":
                        hook["command"] = "exec /bin/sleep 2"
                    elif fault == "collector-saturated":
                        command = shlex.split(hook["command"])[1:]
                        prefix = [sys.executable, str(saturate), str(directory / "saturation")]
                        hook["command"] = "exec " + shlex.join([*prefix, *command])
            hooks[event].extend(groups)
    (config / "hooks.json").write_text(json.dumps({"hooks": hooks}))
    (config / "config.toml").write_text(f'''
model = "gpt-5.4"
model_provider = "fixture"
web_search = "disabled"
approval_policy = "on-request"
sandbox_mode = "read-only"
notify = {json.dumps(observer)}

[model_providers.fixture]
name = "Synthetic loopback"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false

[projects.{json.dumps(str(project))}]
trust_level = "trusted"

[analytics]
enabled = false

[features]
apps = false
plugins = false
plugin_discovery_prefetch = false
remote_plugin = false
''')
    path = os.pathsep.join((str(binary.parent), os.defpath))
    env = {"HOME": str(home), "PATH": path, "SHELL": "/bin/sh", "TERM": "xterm-256color"}
    return trace, env


class FixtureTerminal:
    def __init__(self, directory, trace, name="mux"):
        self.tmux = ["tmux", "-f", "/dev/null", "-S", str(directory / name)]
        self.environment = dict(os.environ)
        self.environment.pop("TMUX", None)
        self.trace = trace

    def start(self, launch, project, env):
        scoped = ["env", "-i", *[f"{key}={value}" for key, value in env.items()], *launch]
        self.call("new-session", "-d", "-s", "fixture", "-x", "116", "-y", "34", "-c", str(project),
                  "exec " + shlex.join([str(value) for value in scoped]))
        self.call("set-option", "-p", "-t", "fixture", "remain-on-exit", "on")

    def close(self):
        subprocess.run([*self.tmux, "kill-server"], env=self.environment, capture_output=True, timeout=3)

    def call(self, *args):
        return subprocess.check_output([*self.tmux, *args], env=self.environment, timeout=3)

    def wait(self, condition, description):
        deadline = time.monotonic() + 8
        while (result := condition()) is None:
            assert time.monotonic() < deadline, f"timed out waiting for {description}"
            time.sleep(0.04)
        return result

    def screen(self, label, *, present=True):
        def visible():
            screen = self.screen_bytes()
            return True if (label in screen) == present else None

        self.wait(visible, label.decode())

    def screen_bytes(self):
        return self.call("capture-pane", "-p", "-t", "fixture")

    def send_line(self, text):
        self.call("send-keys", "-t", "fixture", "-l", text)
        time.sleep(0.3)
        self.send_key("Enter")

    def send_key(self, key):
        self.call("send-keys", "-t", "fixture", key)

    def wait_exit(self):
        def exited():
            status = self.call("display-message", "-p", "-t", "fixture", "#{pane_dead_status}").strip()
            return int(status) if status else None

        assert self.wait(exited, "terminal exit") == 0

    def events(self):
        if not self.trace.exists():
            return []
        payload = self.trace.read_bytes()
        return [json.loads(line) for line in payload[:payload.rfind(b"\n") + 1].splitlines()]

    def event(self, *, after=0, **expected):
        return self.wait(
            lambda: next((event for event in self.events()[after:]
                          if all(event.get(key) == value for key, value in expected.items())), None),
            repr(expected),
        )


def agent_state(terminal, command, /, **expected):
    if command is None:
        return None

    def matching():
        output = subprocess.check_output([*command, "control", "agents"], timeout=3, text=True)
        for line in output.splitlines():
            agent = dict(fields(line), run=line.split()[0])
            if all(agent.get(key) == value for key, value in expected.items()):
                return agent
        return None

    return terminal.wait(matching, repr(expected))


def trust_hooks(terminal):
    trust = b"Trust all and continue"
    terminal.screen(trust)
    terminal.send_key("Down")
    terminal.screen("› 2. Trust all and continue".encode())
    terminal.send_key("Enter")
    terminal.screen(trust, present=False)
    terminal.screen(b"OpenAI Codex")


def exercise(terminal, command, fault, conversation):
    trust_hooks(terminal)
    collector = None if fault else command
    session, initial = exercise_permission(terminal, collector)
    if fault:
        expected_collection = "Healthy" if fault == "timeout" else "NotRegistered"
        agent_state(terminal, command, provider="Codex", collection=expected_collection, response="false")
        if not conversation:
            terminal.send_line("/exit")
            terminal.wait_exit()
            return

    terminal.send_line("/plan")
    terminal.screen(b"Plan mode")
    exercise_question(terminal, collector, session, "answer")
    exercise_question(terminal, collector, session, "cancel")

    if conversation:
        exercise_compaction(terminal, command, session, initial)

    terminal.send_line("/new")
    def choose_context():
        screen = terminal.screen_bytes()
        if b"Use current Git worktree" in screen:
            return True
        return False if b"fixture cancel" not in screen else None

    if terminal.wait(choose_context, "new context chooser"):
        terminal.send_key("Enter")
    terminal.screen(b"fixture cancel", present=False)
    terminal.send_line("fixture response")
    next_start = terminal.wait(
        lambda: next((event for event in terminal.events()
                      if event.get("hook_event_name") == "SessionStart" and event["session_id"] != session), None),
        "new Codex context",
    )
    assert next_start["source"] == "startup"
    terminal.event(hook_event_name="Stop", session_id=next_start["session_id"])
    if initial:
        agent_state(terminal, command, run=initial["run"], collection="GapDetected", process="Running")
    if conversation:
        exercise_interactive_resume(terminal, command, session, initial)
    terminal.send_line("/exit")
    terminal.wait_exit()
    if initial:
        agent_state(terminal, command, run=initial["run"], process="Exited")
    return session, initial


def submit_followup(terminal, prompt, session, source, offset):
    terminal.send_line(prompt)
    start = terminal.event(after=offset, hook_event_name="SessionStart", source=source, session_id=session)
    assert "turn_id" not in start
    turn = terminal.event(after=offset, hook_event_name="UserPromptSubmit", session_id=session)["turn_id"]
    terminal.event(hook_event_name="Stop", session_id=session, turn_id=turn)


def exercise_compaction(terminal, command, session, initial):
    offset = len(terminal.events())
    terminal.send_line("/compact")
    before = terminal.event(hook_event_name="PreCompact", trigger="manual", session_id=session)
    terminal.event(
        hook_event_name="PostCompact", trigger="manual", session_id=session, turn_id=before["turn_id"],
    )
    terminal.screen(b"Context compacted")
    submit_followup(terminal, "fixture after compaction", session, "compact", offset)
    if initial:
        agent_state(
            terminal, command, run=initial["run"], terminal=initial["terminal"], pid=initial["pid"],
            session=session, collection="Healthy", process="Running", resolved="1", cancelled="1",
            unverified="1", pending="0",
        )


def exercise_interactive_resume(terminal, command, session, initial):
    offset = len(terminal.events())
    terminal.send_line(f"/resume {session}")
    terminal.screen(b"fixture after compaction")
    submit_followup(terminal, "fixture after interactive resume", session, "resume", offset)
    if initial:
        agent_state(terminal, command, run=initial["run"], collection="GapDetected", process="Running")


def exercise_resume_launch(terminal, launch, project, env, session, command, previous):
    offset = len(terminal.events())
    terminal.start([*launch, "resume", session], project, env)
    terminal.screen(b"gpt-5.4")
    submit_followup(terminal, "fixture after fresh resume", session, "resume", offset)
    if previous:
        resumed = agent_state(
            terminal, command, provider="Codex", session=session, collection="Healthy", process="Running",
            pending="0", resolved="0", cancelled="0", unverified="0", response="true",
        )
        assert resumed["run"] != previous["run"] and resumed["terminal"] != previous["terminal"]
        assert resumed["pid"] != previous["pid"]
        agent_state(terminal, command, run=previous["run"], process="Exited", resolved="1")
    elif command is not None:
        agent_state(
            terminal, command, provider="Codex", collection="NotRegistered", process="Running",
            pending="0", unverified="0", response="false",
        )
    terminal.send_line("/exit")
    terminal.wait_exit()


def exercise_permission(terminal, command):
    terminal.send_line("fixture approve")
    permission = terminal.event(hook_event_name="PermissionRequest", tool_name="Bash")
    session, turn = permission["session_id"], permission["turn_id"]
    start = terminal.event(hook_event_name="SessionStart", session_id=session, source="startup")
    assert "turn_id" not in start and "tool_use_id" not in permission
    terminal.event(hook_event_name="UserPromptSubmit", session_id=session, turn_id=turn)
    pre = terminal.event(hook_event_name="PreToolUse", session_id=session, turn_id=turn, tool_name="Bash")
    terminal.screen(b"Would you like to run the following command?")
    initial = agent_state(terminal, command, provider="Codex", session=session, collection="Healthy",
                          process="Running", pending="0", unverified="1", adapter="InteractiveCli",
                          version="0.167.0")
    terminal.send_key("Enter")
    terminal.event(hook_event_name="PostToolUse", session_id=session, turn_id=turn, tool_use_id=pre["tool_use_id"])
    terminal.event(hook_event_name="Stop", session_id=session, turn_id=turn, stop_hook_active=False)
    terminal.event(type="agent-turn-complete", **{"thread-id": session, "turn-id": turn})
    agent_state(terminal, command, session=session, response="true", process="Running", unverified="1")
    return session, initial


def exercise_question(terminal, command, session, action):
    terminal.send_line(f"fixture {action}")
    start = terminal.event(hook_event_name="PreToolUse", tool_use_id=f"call_{action}")
    assert start["session_id"] == session and start["tool_name"] == "request_user_input"
    terminal.screen(f"Which synthetic {action} option?".encode())
    agent_state(terminal, command, session=session, pending="0", unverified="2")
    if action == "answer":
        terminal.send_key("Enter")
        terminal.event(hook_event_name="PostToolUse", session_id=session, turn_id=start["turn_id"],
                       tool_use_id=start["tool_use_id"])
        terminal.event(type="agent-turn-complete", **{"thread-id": session, "turn-id": start["turn_id"]})
    else:
        terminal.send_key("Escape")
        terminal.event(hook_event_name="Interrupt", session_id=session, turn_id=start["turn_id"])
    agent_state(terminal, command, session=session, process="Running", unverified="1", resolved="1",
                cancelled="0" if action == "answer" else "1")


def parallel_questions(first, second, command, directory):
    first.send_line("/plan")
    first.screen(b"Plan mode")
    first.send_line("fixture answer")
    first.screen(b"Which synthetic answer option?")
    before = first.event(hook_event_name="PreToolUse", tool_use_id="call_answer")
    one = agent_state(first, command, session=before["session_id"], unverified="1", resolved="0")

    second.send_line("/plan")
    second.screen(b"Plan mode")
    second.send_line("fixture answer")
    second.screen(b"Which synthetic answer option?")
    another = second.wait(
        lambda: next((event for event in second.events()
                      if event.get("tool_use_id") == "call_answer"
                      and event["session_id"] != before["session_id"]), None),
        "second question with an independent session",
    )
    two = agent_state(second, command, session=another["session_id"], unverified="1", resolved="0")
    for key in ("run", "terminal", "pid", "session"):
        assert one[key] != two[key]

    companion = Companion(command, directory)
    try:
        focus_and_ack_question(first, second, command, companion, one, two)
    finally:
        companion.close()

    first.send_key("Enter")
    agent_state(first, command, run=one["run"], unverified="0", resolved="1")
    agent_state(second, command, run=two["run"], unverified="1", resolved="0")
    second.screen(b"Which synthetic answer option?")
    second.send_key("Escape")
    agent_state(second, command, run=two["run"], unverified="0", resolved="0", cancelled="1")


def focus_and_ack_question(first, second, command, companion, one, two):
    companion.start()
    first.send_key("C-]")
    first.send_key("r")
    first.screen(b"Controller pairing code")
    code = re.search(rb"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", first.screen_bytes())[0]
    pair = fields(companion.run("pair", input=code.decode() + "\n").stdout)
    first.send_key("Enter")
    first.screen(b"Which synthetic answer option?")

    def presented(view):
        runs = {slot.get("run", "").rsplit("/", 1)[-1] for slot in view["slots"]}
        return view["bindings"] and {one["run"], two["run"]} <= runs

    view = companion.wait(presented)
    slot = next(slot for slot in view["slots"] if slot.get("run", "").endswith("/" + one["run"]))
    binding, = view["bindings"]
    assert binding["client_id"] == pair["client_id"]
    first.send_key("C-]")
    first.send_key("t")

    def other_tab():
        clients = subprocess.check_output([*command, "control", "clients"], text=True, timeout=3)
        for line in clients.splitlines():
            if line.split()[0] == pair["client_id"] and fields(line).get("terminal") != one["terminal"]:
                return True
        return None

    first.wait(other_tab, "first TUI switches to its shell tab")
    applied = companion.run("focus-slot", "--slot", slot["capture"], "--binding", binding["capture"])
    assert fields(applied.stdout)["terminal_id"] == one["terminal"]
    first.screen(b"Which synthetic answer option?")
    second.screen(b"Which synthetic answer option?")
    companion.run("ack", "--control", view["header"]["control_capture"], "--run", slot["run"],
                  "--attention-id", slot["latest_attention_id"])
    agent_state(first, command, run=one["run"], unverified="1", resolved="0")
    first.screen(b"Which synthetic answer option?")


def capture(binary, corvid, directory, port, fault, auto_start, parallel, conversation):
    project = directory / "project"
    project.mkdir()
    trace, env = configure(directory, project, port, binary, corvid, fault)
    daemon = command = None
    launch = [binary]
    terminal = FixtureTerminal(directory, trace)
    second = FixtureTerminal(directory, trace, "mux-other") if parallel else None
    try:
        if corvid is not None:
            runtime = directory / "run"
            terminfo = directory / "terminfo"
            subprocess.run(["tic", "-x", "-o", terminfo, ROOT / "spikes/terminal/corvid.terminfo"],
                           check=True, timeout=3)
            env["TERMINFO"] = str(terminfo)
            command = [corvid, "--runtime-dir", runtime]
            daemon = subprocess.Popen([*command, "daemon", "--state-dir", directory / "state"],
                                      env=env, cwd=project)
            wait_for_daemon(command, daemon)
            if fault == "missing-collector":
                (runtime / "collector.sock").unlink()
            launch = [*command, "attach", "--agent", "codex"]
        if auto_start is not None:
            if command is not None:
                launch.append("--")
            launch.extend(["--enable" if auto_start else "--disable", "daemon_auto_start"])
        terminal.start(launch, project, env)
        if second is None:
            try:
                result = exercise(terminal, command, fault, conversation)
                if conversation:
                    second = FixtureTerminal(directory, trace, "mux-resume")
                    session, previous = result
                    resume = launch if command is None or "--" in launch else [*launch, "--"]
                    exercise_resume_launch(second, resume, project, env, session, command, previous)
            except AssertionError:
                if conversation:
                    active = second or terminal
                    print(active.screen_bytes().decode(errors="replace"), file=sys.stderr)
                    sanitize(terminal.events())
                raise
        else:
            trust_hooks(terminal)
            second.start(
                [*command, "attach", "--agent", "codex", "--new-workspace", "Other"], project, env,
            )
            second.screen(b"gpt-5.4")
            parallel_questions(terminal, second, command, directory)
            for tty in (terminal, second):
                tty.send_line("/exit")
                tty.wait_exit()
        events = terminal.events()
        if fault == "collector-saturated":
            assert (directory / "saturation").read_text().splitlines() == ["32 handshakes"] * 6
    finally:
        terminal.close()
        if second is not None:
            second.close()
        if daemon is not None:
            stop(daemon)
        if auto_start is not False:
            stop_local_broker(Path(env["HOME"]))
    assert events and all(event["parent"] == "codex" for event in events)
    return events


def stop_local_broker(home):
    private_package = home / ".codex/packages/app-server-daemon"
    handles = []
    try:
        for entry in Path("/proc").glob("[0-9]*/exe"):
            try:
                if not entry.readlink().is_relative_to(private_package):
                    continue
                handle = os.pidfd_open(int(entry.parent.name))
                try:
                    if entry.readlink().is_relative_to(private_package):
                        handles.append(handle)
                        handle = None
                finally:
                    if handle is not None:
                        os.close(handle)
            except OSError:
                continue
        for handle in handles:
            try:
                signal.pidfd_send_signal(handle, signal.SIGTERM)
                if not select.select([handle], [], [], 2)[0]:
                    signal.pidfd_send_signal(handle, signal.SIGKILL)
                    assert select.select([handle], [], [], 2)[0], "isolated broker did not exit"
            except ProcessLookupError:
                pass
    finally:
        for handle in handles:
            os.close(handle)


def sanitize(events):
    ids = {}
    for event in events:
        for field in ("session_id", "turn_id", "thread-id", "turn-id"):
            if field in event:
                event[field] = ids.setdefault(event[field], f"id-{len(ids) + 1}")
        print(json.dumps(event, separators=(",", ":")))


def main():
    from itertools import count

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corvid", type=Path)
    parser.add_argument("--fault", choices=("missing-helper", "missing-collector", "timeout", "collector-saturated"))
    broker = parser.add_mutually_exclusive_group()
    broker.add_argument("--disable-auto-start", dest="auto_start", action="store_false")
    broker.add_argument("--enable-auto-start", dest="auto_start", action="store_true")
    parser.set_defaults(auto_start=None)
    parser.add_argument("--parallel", action="store_true")
    parser.add_argument("--conversation", action="store_true")
    args = parser.parse_args()
    if args.fault and not args.corvid:
        parser.error("--fault requires --corvid")
    if args.parallel and (not args.corvid or args.fault):
        parser.error("--parallel requires a managed Corvid fixture without fault injection")
    if args.parallel and args.auto_start is True:
        parser.error("--parallel requires independent Codex processes")
    if args.conversation and (args.parallel or args.fault not in (None, "missing-helper", "missing-collector")):
        parser.error("--conversation supports missing-helper or missing-collector for a single CLI")
    executable = shutil.which("codex")
    assert executable, "codex must be installed on PATH"
    binary = Path(executable).resolve()
    version = subprocess.check_output([binary, "--version"], text=True, timeout=3).strip()
    assert version == VERSION, f"expected {VERSION}, found {version}"
    corvid = args.corvid.resolve() if args.corvid else None
    with tempfile.TemporaryDirectory(prefix=".corvid-codex-", dir=ROOT if corvid else None) as temporary:
        with ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture) as server:
            server.sequence = count(1)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            try:
                events = capture(
                    binary, corvid, Path(temporary), server.server_port, args.fault,
                    args.auto_start, args.parallel, args.conversation,
                )
            finally:
                server.shutdown()
                worker.join()
    sanitize(events)


if __name__ == "__main__":
    main()
