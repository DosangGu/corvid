"""Keep managed Claude/Codex processes and collection across daemon replacements."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import tempfile

from interactive import OuterTerminal, stop, wait_for_daemon


def fields(output):
    return dict(item.split("=", 1) for item in output.split() if "=" in item)


def prepare(command, runtime, schema):
    def transcode(operation, payload):
        return subprocess.check_output(
            ["protoc", "-I", schema.parent, f"--{operation}=corvid.protocol.v1.Envelope", schema.name],
            input=payload, timeout=3,
        )

    identity = fields(subprocess.check_output([*command, "control", "host"], text=True))
    scope = f'protocol_major: 1 host_id: "{identity["host_id"]}" server_epoch: "{identity["server_epoch"]}"'
    messages = [
        'protocol_major: 1 request_id: "hello" hello { supported_major_versions: 1 required_capabilities: "daemon_handoff.v1" }',
        f'{scope} request_id: "upgrade" upgrade_daemon {{ preserve_terminals: true }}',
    ]
    payload = b"CRV1" + b"".join(
        struct.pack(">IB", len(encoded) + 1, 1) + encoded
        for encoded in (transcode("encode", message.encode()) for message in messages)
    )
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(runtime / "upgrade.sock"))
        connection.sendall(payload)
        with connection.makefile("rb") as response:
            assert response.read(4) == b"CRV1"
            replies = []
            for _ in messages:
                length, kind = struct.unpack(">IB", response.read(5))
                assert kind == 1 and length < 65536
                replies.append(transcode("decode", response.read(length - 1)))
    assert b"daemon_upgrade_ready" in replies[-1], replies[-1]
    return identity


def verify(binary, directory, terminfo, ssh=None, *, legacy=False, managed=False, broker=False,
           previous=None, providers=("claude", "codex")):
    repository = Path(__file__).resolve().parents[3]
    # Keep Unix socket names within sockaddr_un even below the SSH fixture root.
    root = directory / f"a{int(legacy)}{int(managed)}{int(broker)}{''.join(name[0] for name in providers)}"
    root.mkdir(mode=0o700)
    candidate = binary
    if previous:
        binary = root / "corvid"
        shutil.copy2(previous, binary)
    programs = root / "bin"
    programs.mkdir()
    home = root / "home"
    home.mkdir()
    shell = root / "shell"
    shell.write_text("#!/bin/sh\nPS1='HANDOFF-SHELL> ' exec /bin/sh -i\n")
    shell.chmod(0o700)
    build_env = os.environ.copy()
    versions = (repository / "crates/corvid-protocol/src/collector.rs").read_text()
    for provider in ("CLAUDE", "CODEX"):
        build_env[f"CORVID_FIXTURE_{provider}"] = re.search(rf'{provider}_VERSION: &str = "([^"]+)"', versions)[1]
    subprocess.run(
        ["rustc", "--edition=2024", Path(__file__).with_name("fixtures") / "shell_agent.rs", "-o", programs / "claude"],
        env=build_env, check=True,
    )
    (programs / "codex").hardlink_to(programs / "claude")
    env = os.environ | {
        "HOME": str(home), "SHELL": str(shell), "TERMINFO": str(terminfo), "TERM": "xterm-256color",
        "PATH": str(programs), "CORVID_FIXTURE_BINARY": str(binary), "CORVID_FIXTURE_HANDOFF": "1",
        "CORVID_AGENT_MONITOR": "0", "CORVID_VIEW_STATE_DIR": str(root / "views"),
    }
    for key in ("ENV", "BASH_ENV", "ZDOTDIR", "CORVID_FIXTURE_BROKER"):
        env.pop(key, None)
    if broker:
        target = f"{os.uname().machine}-unknown-linux-musl"
        version = build_env["CORVID_FIXTURE_CODEX"]
        executable = root / "packages/app-server-daemon/releases" / f"{version}-{target}/bin/codex"
        executable.parent.mkdir(parents=True)
        shutil.copy2(programs / "codex", executable)
        env["CORVID_FIXTURE_BROKER"] = str(executable)
    runtime = root / "run"
    command = [binary, "--runtime-dir", runtime]
    def control(verb):
        args = [*command, "control", verb]
        if ssh:
            args = [*ssh, "-T", shlex.join(map(str, args))]
        return subprocess.check_output(args, text=True, timeout=5)

    def start():
        daemon = subprocess.Popen([*command, "daemon", "--state-dir", root / "state", "--autostarted"], env=env)
        wait_for_daemon(command, daemon)
        return daemon

    daemon = start()
    try:
        with ExitStack() as stack:
            for provider in providers:
                args = [*command, "attach", "--new"]
                if managed:
                    args += ["--agent", provider]
                def attach():
                    remote = [*ssh, "-tt", shlex.join(["env", "CORVID_AGENT_MONITOR=0", *map(str, args)])] if ssh else args
                    return stack.enter_context(OuterTerminal(remote, env=os.environ | {"TERM": "xterm-256color", "CORVID_VIEW_STATE_DIR": str(root / "views")}))
                terminal = attach()
                args = [*command, "attach"]
                if not managed:
                    terminal.expect(b"HANDOFF-SHELL>")
                    terminal.send(f"{provider}\n".encode())
                pid = terminal.expect(rb"RETAINED-" + provider.encode() + rb"-PID=(\d+)")[1]
                original = control("agents")
                old_run = next(line.split()[0] for line in original.splitlines() if f"provider={provider.title()} process=Running" in line)
                owner_pid = fields(next(line for line in original.splitlines() if line.startswith(old_run + " ")))["pid"]
                # Leave a question pending at replacement; it must remain historical.
                terminal.send(b"question\n")
                terminal.expect(b"OBSERVED-question-PID=" + pid)
                assert "request_kind=Question" in control("attention")
                for generation in range(2):
                    before = prepare(command, runtime, repository / "crates/corvid-protocol/proto/corvid.proto")
                    assert daemon.wait(timeout=5) == 0
                    terminal.process.wait(timeout=5)
                    saved = runtime / "handoff.json"
                    payload = json.loads(saved.read_text())
                    retained_agent = next((terminal["agent"] for _, terminal in payload["terminals"] if terminal.get("agent")), None)
                    if retained_agent is None:
                        # Only the isolated fixture's known provider process is read.
                        keys = {b"CORVID_LAUNCH_ASSOCIATION": "association", b"CORVID_SERVER_EPOCH": "observer_epoch"}
                        retained_agent = {keys[key]: value.decode() for key, separator, value in
                                          (entry.partition(b"=") for entry in (Path("/proc") / owner_pid / "environ").read_bytes().split(b"\0"))
                                          if separator and key in keys}
                    if previous and generation == 0:
                        # Reproduce monitoring already lost during a v0.2.5-to-v0.2.5
                        # replacement, then update the same executable path in place.
                        daemon = start()
                        assert not control("agents")
                        prepare(command, runtime, repository / "crates/corvid-protocol/proto/corvid.proto")
                        assert daemon.wait(timeout=5) == 0
                        staged = root / "corvid.new"
                        shutil.copy2(candidate, staged)
                        os.replace(staged, binary)
                        payload = json.loads(saved.read_text())
                    if legacy and generation == 0:
                        for _, retained in payload["terminals"]:
                            retained.pop("agent", None)
                            retained.pop("agent_handoff", None)
                        saved.write_text(json.dumps(payload))
                    daemon = start()
                    after = fields(control("host"))
                    assert after["host_id"] == before["host_id"] and after["server_epoch"] != before["server_epoch"]
                    agents = control("agents")
                    current = next(line for line in agents.splitlines() if f"provider={provider.title()} process=Running" in line)
                    run = current.split()[0]
                    assert run != old_run and f"pid={owner_pid}" in current, current
                    assert "collection=Degraded" in current, current
                    assert not control("attention"), "old requests became live again"
                    forged_env = os.environ | {
                        "CORVID_RUNTIME_DIR": str(runtime), "CORVID_HOST_ID": after["host_id"],
                        "CORVID_LAUNCH_ASSOCIATION": retained_agent["association"],
                        "CORVID_SERVER_EPOCH": retained_agent["observer_epoch"],
                    }
                    forged = {"hook_event_name": "PreToolUse", "session_id": f"fixture-{provider}",
                              "tool_name": "AskUserQuestion" if provider == "claude" else "request_user_input",
                              "tool_use_id": "forged", "prompt_id": "turn-1", "turn_id": "turn-1"}
                    subprocess.run([binary, "observe", "--provider", provider], input=json.dumps(forged),
                                   text=True, env=forged_env, check=True, timeout=3, stdout=subprocess.DEVNULL)
                    assert not control("attention"), "unowned peer reused retained observer scope"
                    terminal = attach()
                    terminal.expect(b"RETAINED-" + provider.encode() + b"-PID=" + pid)
                    terminal.output.clear()
                    terminal.send(b"wrong-epoch\n")
                    terminal.expect(b"OBSERVED-wrong-epoch-PID=" + pid)
                    assert not control("attention"), "foreign observer epoch accepted"
                    terminal.output.clear()
                    terminal.send(b"question\n")
                    terminal.expect(b"OBSERVED-question-PID=" + pid)
                    assert f"run_id={run} " in control("attention") and "request_kind=Question" in control("attention")
                    terminal.send(b"respond\n")
                    terminal.expect(b"OBSERVED-respond-PID=" + pid)
                    assert "reason=Response" in control("attention")
                    assert "collection=Healthy" in control("agents")
                    old_run = run
                terminal.send(b"exit\n")
                if not managed:
                    terminal.expect(b"HANDOFF-SHELL>")
                    terminal.send(b"exit\n")
                terminal.wait(0)
    finally:
        stop(daemon)
    print(f"PASS retained {providers} PID, questions, responses, epoch fencing and repeated handoff (legacy={legacy}, managed={managed}, broker={broker}, ssh={ssh is not None})", flush=True)


def run(binary, previous=None):
    binary = binary.resolve()
    repository = Path(__file__).resolve().parents[3]
    with tempfile.TemporaryDirectory(prefix="corvid-agent-handoff-", dir=Path.home()) as temporary:
        directory = Path(temporary)
        terminfo = directory / "terminfo"
        subprocess.run(["tic", "-x", "-o", terminfo, repository / "crates/corvid-tui/assets/corvid.terminfo"], check=True)
        if previous:
            for provider in ("claude", "codex"):
                verify(binary, directory, terminfo, legacy=True, previous=previous, providers=(provider,))
            return
        for legacy in ((False, True) if sys.platform == "linux" else (False,)):
            for managed in (False, True):
                verify(binary, directory, terminfo, legacy=legacy, managed=managed)
            if sys.platform == "linux":
                verify(binary, directory, terminfo, legacy=legacy, broker=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--previous-binary", type=Path, help="also recover monitoring already lost by this older Linux runtime")
    args = parser.parse_args()
    run(args.binary, args.previous_binary)
