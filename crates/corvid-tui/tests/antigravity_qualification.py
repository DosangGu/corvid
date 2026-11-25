"""Qualify installed Antigravity CLI (agy 1.5.17) in Corvid PTYs using a loopback model."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

from collector_fixture import SCRIPT as COLLECTOR_FIXTURE, verify_crashed_hooks
from companion_fixture import Companion, fields
from interactive import OuterTerminal, stop, wait_for_daemon

ROOT = Path(__file__).resolve().parents[3]
CRASH_TRACE = "crashed-hooks"


class ModelFixture(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers["Content-Length"])
        body = json.loads(self.rfile.read(length))
        contents = body.get("contents", [])
        text_in = json.dumps(contents)
        scenario = self.server.scenario

        if scenario == "error" or "trigger error" in text_in:
            self.send_response(500)
            payload = json.dumps({"error": {"code": 500, "message": "fixture 500 error"}}).encode()
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        if scenario == "basic":
            if self.server.hold_first and self.server.requests == 0:
                self.server.first_entered.set()
                assert self.server.release_first.wait(timeout=10), "timed out releasing held response"
            self.server.requests += 1
            has_func_resp = any(
                "functionResponse" in part
                for content in contents
                for part in content.get("parts", [])
            )
            if "run echo" in text_in and not has_func_resp:
                parts = [{
                    "functionCall": {
                        "name": "run_command",
                        "args": {
                            "CommandLine": "printf CORVID_AGY_TOOL",
                            "WaitMsBeforeAsync": 5000,
                            "toolSummary": "Print tool marker",
                        },
                    }
                }]
            else:
                parts = [{"text": "Antigravity synthetic response"}]
        elif scenario == "subagent":
            has_func_resp = any(
                "functionResponse" in part
                for content in contents
                for part in content.get("parts", [])
            )
            if "hello subagent" in text_in and "delegate task" not in text_in:
                parts = [{"text": "Subagent finished"}]
            elif not has_func_resp:
                parts = [{
                    "functionCall": {
                        "name": "invoke_subagent",
                        "args": {
                            "TypeName": "research",
                            "Role": "Test Researcher",
                            "Prompt": "hello subagent",
                        },
                    }
                }]
            else:
                parts = [{"text": "Parent finished"}]
        else:
            parts = [{"text": "Antigravity synthetic response"}]

        payload = {
            "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15,
            },
        }
        if "alt=sse" in self.path:
            out = f"data: {json.dumps(payload)}\n\n".encode()
            content_type = "text/event-stream"
        else:
            out = json.dumps([payload] if "streamGenerateContent" in self.path else payload).encode()
            content_type = "application/json"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


def start_model(scenario, *, hold_first=False):
    server = ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture)
    server.scenario = scenario
    server.requests = 0
    server.hold_first = hold_first
    server.first_entered = threading.Event()
    server.release_first = threading.Event()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def setup_home(directory, project, port, corvid, *, install_hooks=True, fault=None):
    home = directory / "home"
    cli_dir = home / ".gemini" / "antigravity-cli"
    cli_dir.mkdir(parents=True, mode=0o700)
    (cli_dir / "settings.json").write_text(json.dumps({
        "modelProvider": "gemini",
        "model": "gemini-2.5-flash",
        "toolPermission": "always-proceed",
        "trustedWorkspaces": [str(project)],
    }))
    env = dict(os.environ)
    env["HOME"] = str(home)
    env["GEMINI_API_KEY"] = "fixture-key"
    env["GOOGLE_GEMINI_BASE_URL"] = f"http://127.0.0.1:{port}"
    env.setdefault("TERM", "xterm-256color")

    if install_hooks:
        subprocess.run(
            [corvid, "hooks", "antigravity", "--install"],
            env=env, check=True, capture_output=True, text=True, timeout=5,
        )
        hooks_path = home / ".gemini" / "config" / "hooks.json"
        assert hooks_path.exists()
        if fault is not None:
            hooks_cfg = json.loads(hooks_path.read_text())
            entry = hooks_cfg["corvid-observer"]
            for event, handlers in entry.items():
                for handler in handlers:
                    if fault == "missing-helper":
                        handler["command"] = "exec " + shlex.quote(str(directory / "missing-corvid"))
                    elif fault == "missing-collector":
                        handler["command"] = (
                            "env CORVID_RUNTIME_DIR="
                            + shlex.quote(str(directory / "missing-runtime"))
                            + " "
                            + handler["command"]
                        )
                    elif fault == "helper-crash":
                        handler["command"] = "exec " + shlex.join([
                            sys.executable, "-B", str(COLLECTOR_FIXTURE),
                            "crash", str(directory / CRASH_TRACE), event,
                        ])
            hooks_path.write_text(json.dumps(hooks_cfg))
    return home, env


def wait_for(predicate, description, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value is not None:
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {description}")


def agent_state(command, **expected):
    def matching():
        output = subprocess.check_output([*command, "control", "agents"], timeout=3, text=True)
        for line in output.splitlines():
            agent = dict(fields(line), run=line.split()[0])
            if all(agent.get(key) == value for key, value in expected.items()):
                return agent
        return None

    return wait_for(matching, repr(expected))


def run_agy_via_corvid(corvid, runtime, env, project, args, *, wait_exit=True):
    command = [
        corvid, "--runtime-dir", str(runtime),
        "attach", "--new-workspace", f"ws-{len(args)}-{int(time.monotonic() * 1000)}",
        "--workspace-directory", str(project),
        "--agent", "antigravity", "--", *args,
    ]
    return OuterTerminal(command, cols=110, rows=32, env=env)


def expect_plain(term, needle, timeout=8):
    ansi_re = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        plain = ansi_re.sub(b"", bytes(term.output))
        if needle in plain:
            return
        term.read_output(min(0.05, max(0.0, deadline - time.monotonic())))
    raise AssertionError((needle, ansi_re.sub(b"", bytes(term.output))[-500:]))


def run_basic_and_resume(corvid, agy_bin, directory):
    project = directory / "project"
    project.mkdir()
    runtime = directory / "runtime"
    state = directory / "state"
    server = start_model("basic", hold_first=True)
    home, env = setup_home(directory, project, server.server_address[1], corvid, install_hooks=True)
    env["PATH"] = f"{Path(agy_bin).parent}{os.pathsep}{os.defpath}"
    base_cmd = [corvid, "--runtime-dir", str(runtime)]
    daemon = subprocess.Popen(
        [*base_cmd, "daemon", "--state-dir", str(state)],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    companion = None
    try:
        wait_for_daemon(base_cmd, daemon)
        companion = Companion(base_cmd, directory, env=env)
        companion.start()

        # Run 1: held model call proves Working badge before completion, then ResponseObserved.
        with OuterTerminal(
            [
                *base_cmd, "attach", "--new-workspace", "basic",
                "--workspace-directory", str(project),
                "--agent", "agy", "--", "-p", "run echo", "--output-format", "json",
            ],
            cols=110, rows=32, env=env,
        ) as term:
            assert server.first_entered.wait(timeout=10), "agy did not reach mock model"
            working = agent_state(
                base_cmd, provider="Antigravity", collection="Healthy",
                badge="Working", process="Running", adapter="InteractiveCli", version="1.5.17",
            )
            session_id = working["session"]
            server.release_first.set()
            expect_plain(term, b"Antigravity synthetic response")
            expect_plain(term, b'"status":"SUCCESS"')
            agent_state(
                base_cmd, run=working["run"], provider="Antigravity",
                collection="Healthy", response="true",
            )
            companion.wait(lambda view: any(
                slot.get("run", "").endswith("/" + working["run"])
                for slot in view["slots"]
            ))
            term.wait(0)

        agent_state(base_cmd, run=working["run"], process="Exited")

        # Run 2: resume with --conversation (no SessionStart emitted; binds on PreInvocation).
        server.hold_first = False
        with OuterTerminal(
            [
                *base_cmd, "attach", "--new-workspace", "resume",
                "--workspace-directory", str(project),
                "--agent", "antigravity", "--",
                "-p", "follow up", "--conversation", session_id, "--output-format", "json",
            ],
            cols=110, rows=32, env=env,
        ) as term:
            expect_plain(term, b"Antigravity synthetic response")
            expect_plain(term, b'"status":"SUCCESS"')
            resumed = agent_state(
                base_cmd, provider="Antigravity", session=session_id,
                collection="Healthy", response="true", process="Running",
            )
            assert resumed["run"] != working["run"]
            term.wait(0)
        agent_state(base_cmd, run=resumed["run"], process="Exited")
    finally:
        if companion is not None:
            companion.close()
        stop(daemon)
        server.shutdown()


def run_subagent_and_error(corvid, agy_bin, directory, scenario):
    project = directory / f"project-{scenario}"
    project.mkdir()
    runtime = directory / f"runtime-{scenario}"
    state = directory / f"state-{scenario}"
    server = start_model(scenario)
    _, env = setup_home(directory, project, server.server_address[1], corvid, install_hooks=True)
    env["PATH"] = f"{Path(agy_bin).parent}{os.pathsep}{os.defpath}"
    base_cmd = [corvid, "--runtime-dir", str(runtime)]
    daemon = subprocess.Popen(
        [*base_cmd, "daemon", "--state-dir", str(state)],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        wait_for_daemon(base_cmd, daemon)
        prompt = "delegate task" if scenario == "subagent" else "trigger error"
        with OuterTerminal(
            [
                *base_cmd, "attach", "--new-workspace", scenario,
                "--workspace-directory", str(project),
                "--agent", "antigravity", "--", "-p", prompt, "--output-format", "json",
            ],
            cols=110, rows=32, env=env,
        ) as term:
            if scenario == "subagent":
                expect_plain(term, b'"status":"SUCCESS"')
                run = agent_state(
                    base_cmd, provider="Antigravity", collection="Healthy", response="true",
                )
                assert run["collection"] == "Healthy"
                term.wait(0)
            else:
                expect_plain(term, b'"status":"ERROR"')
                agent_state(
                    base_cmd, provider="Antigravity", collection="Healthy", badge="Error",
                )
                term.wait(3)
    finally:
        stop(daemon)
        server.shutdown()


def run_osc7501_without_hooks(corvid, agy_bin, directory):
    project = directory / "project-osc"
    project.mkdir()
    runtime = directory / "runtime-osc"
    state = directory / "state-osc"
    server = start_model("osc")
    _, env = setup_home(directory, project, server.server_address[1], corvid, install_hooks=False)
    env["PATH"] = f"{Path(agy_bin).parent}{os.pathsep}{os.defpath}"
    base_cmd = [corvid, "--runtime-dir", str(runtime)]
    daemon = subprocess.Popen(
        [*base_cmd, "daemon", "--state-dir", str(state)],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        wait_for_daemon(base_cmd, daemon)
        with OuterTerminal(
            [
                *base_cmd, "attach", "--new-workspace", "osc",
                "--workspace-directory", str(project),
                "--agent", "antigravity",
            ],
            cols=110, rows=32, env=env,
        ) as term:
            expect_plain(term, b"Antigravity CLI")
            expect_plain(term, b"App waiting for an answer")

            def saw_program_status():
                out = subprocess.check_output([*base_cmd, "control", "snapshot"], timeout=3, text=True)
                agents_out = subprocess.check_output([*base_cmd, "control", "agents"], timeout=3, text=True)
                return ("Antigravity" in agents_out and "NotRegistered" in agents_out and "agents=1" in out) or None

            wait_for(saw_program_status, "unhooked Antigravity run")
            term.send(b"\x03")
    finally:
        stop(daemon)
        server.shutdown()


def run_fault(corvid, agy_bin, directory, fault):
    project = directory / f"project-{fault}"
    project.mkdir()
    runtime = directory / f"runtime-{fault}"
    state = directory / f"state-{fault}"
    server = start_model("basic")
    _, env = setup_home(
        directory, project, server.server_address[1], corvid,
        install_hooks=True, fault=fault,
    )
    env["PATH"] = f"{Path(agy_bin).parent}{os.pathsep}{os.defpath}"

    base_cmd = [corvid, "--runtime-dir", str(runtime)]
    daemon = subprocess.Popen(
        [*base_cmd, "daemon", "--state-dir", str(state)],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        wait_for_daemon(base_cmd, daemon)
        with OuterTerminal(
            [
                *base_cmd, "attach", "--new-workspace", fault,
                "--workspace-directory", str(project),
                "--agent", "antigravity", "--", "-p", "run echo", "--output-format", "json",
            ],
            cols=110, rows=32, env=env,
        ) as term:
            expect_plain(term, b"Antigravity synthetic response")
            expect_plain(term, b'"status":"SUCCESS"')
            agent_state(base_cmd, provider="Antigravity", collection="NotRegistered", response="false")
            term.wait(0)
        if fault == "helper-crash":
            verify_crashed_hooks(
                directory / CRASH_TRACE,
                {"SessionStart", "PreInvocation", "PostInvocation", "Stop"},
            )
    finally:
        stop(daemon)
        server.shutdown()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--corvid", default=str(ROOT / "target/debug/corvid"))
    parser.add_argument("--agy", default=shutil.which("agy") or "")
    parser.add_argument(
        "--scenario",
        choices=("all", "basic", "subagent", "error", "osc7501"),
        default="all",
    )
    parser.add_argument(
        "--fault",
        choices=("missing-helper", "missing-collector", "helper-crash"),
    )
    args = parser.parse_args()
    if not args.agy:
        raise SystemExit("agy binary not found on PATH; pass --agy")

    with tempfile.TemporaryDirectory(prefix="corvid-agy-qual-", dir=Path.home()) as tmp:
        directory = Path(tmp)
        if args.fault is not None:
            run_fault(args.corvid, args.agy, directory, args.fault)
        elif args.scenario == "all":
            (directory / "basic").mkdir()
            run_basic_and_resume(args.corvid, args.agy, directory / "basic")
            (directory / "subagent").mkdir()
            run_subagent_and_error(args.corvid, args.agy, directory / "subagent", "subagent")
            (directory / "error").mkdir()
            run_subagent_and_error(args.corvid, args.agy, directory / "error", "error")
            (directory / "osc").mkdir()
            run_osc7501_without_hooks(args.corvid, args.agy, directory / "osc")
            for fault in ("missing-helper", "missing-collector", "helper-crash"):
                sub = directory / f"fault-{fault}"
                sub.mkdir()
                run_fault(args.corvid, args.agy, sub, fault)
        elif args.scenario == "basic":
            run_basic_and_resume(args.corvid, args.agy, directory)
        elif args.scenario in ("subagent", "error"):
            run_subagent_and_error(args.corvid, args.agy, directory, args.scenario)
        elif args.scenario == "osc7501":
            run_osc7501_without_hooks(args.corvid, args.agy, directory)
    print("antigravity qualification passed")


if __name__ == "__main__":
    main()
