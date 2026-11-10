"""Qualify Corvid across an actual reboot of a disposable Linux QEMU guest."""

import argparse
import os
from pathlib import Path
import re
import shlex
import subprocess
import time
import uuid

from companion_fixture import fields
from interactive import OuterTerminal
from last_login import pinned_ssh, remote
from ssh_lifecycle import relay_exchange, transcode


BOOT_ID = "cat /proc/sys/kernel/random/boot_id"
CONTROLLER = "corvid-reboot-fixture"
PAIRING_CODE = rb"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"


def control(operator, corvid, request):
    return remote(operator, shlex.join([corvid, "control", request]))


def register(operator, corvid, terminal, scope):
    terminal.output.clear()
    terminal.send(b"\x1dr")
    terminal.expect(b"Controller pairing code")
    code = terminal.expect(PAIRING_CODE)[0].decode()
    command = [
        corvid, "controller",
        "--controller-id", CONTROLLER,
        "--host-id", scope["host_id"],
        "--server-epoch", scope["server_epoch"],
        "pair",
    ]
    binding = fields(remote(operator, shlex.join(command), data=code + "\n"))
    terminal.output.clear()
    terminal.send(b"\r")
    terminal.expect(b"REBOOT-READY")
    return binding


def focus_payloads(schema, scope, binding, run, request_id):
    hello = (
        'protocol_major: 1 request_id: "hello" '
        f'hello {{ controller_id: "{CONTROLLER}" supported_major_versions: 1 '
        'required_capabilities: "focus_agent.v1" }'
    )
    focus = (
        f'protocol_major: 1 request_id: "{request_id}" '
        f'host_id: "{scope["host_id"]}" server_epoch: "{scope["server_epoch"]}" '
        f'focus_agent {{ controller_id: "{CONTROLLER}" client_id: "{binding["client_id"]}" '
        f'agent_run_id: "{run}" binding_revision: {binding["binding_revision"]} }}'
    )
    return [transcode(schema, "encode", message.encode()) for message in (hello, focus)]


def read_field(reply, name):
    match = re.search(rf'^[ \t]*{name}: "([^"\n]+)"$', reply, re.MULTILINE)
    assert match is not None, reply
    return match[1]


def replay_focus(operator, corvid, schema, payloads, scope, request_id):
    greeting, result = relay_exchange(operator, [corvid], schema, payloads)
    greeting, result = greeting.decode(), result.decode()
    assert "hello_result {" in greeting, greeting
    for reply in (greeting, result):
        assert read_field(reply, "host_id") == scope["host_id"], reply
        assert read_field(reply, "server_epoch") == scope["server_epoch"], reply
    assert read_field(result, "request_id") == request_id, result
    return result


def wait_for_save(operator, corvid):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        status = fields(control(operator, corvid, "status"))
        if status["persistence"] == "saved":
            return
        assert status["persistence"] != "failed", status
        time.sleep(0.1)
    raise AssertionError("daemon metadata was not saved")


def require_disposable_guest(operator, observer):
    command = f"id -u; {BOOT_ID}; systemd-detect-virt --vm"
    uid, boot, vm = remote(operator, command).splitlines()
    observer_uid, observer_boot, observer_vm = remote(observer, command).splitlines()
    if uid == observer_uid or boot != observer_boot or vm != observer_vm or vm not in {"qemu", "kvm"}:
        raise RuntimeError("use two accounts on the same disposable QEMU guest")
    return boot


def reboot_guest(operator, observer, corvid, boot):
    # Schedule the reboot so the SSH command can return before its connection closes.
    remote(observer, "sudo -n systemd-run --quiet --on-active=2s /usr/bin/systemctl reboot")
    ping = shlex.join([corvid, "control", "ping"])
    command = f"{BOOT_ID} && {ping} >/dev/null"
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        try:
            probe = subprocess.run(
                [*operator, "-T", command], capture_output=True, text=True, timeout=4
            )
        except subprocess.TimeoutExpired:
            continue
        if probe.returncode == 0 and probe.stdout.strip() != boot:
            return probe.stdout.strip()
        time.sleep(0.25)
    raise AssertionError("the guest did not return with a new kernel boot ID and a live daemon")


def require_empty(operator, corvid):
    snapshot = fields(control(operator, corvid, "snapshot"))
    for name in ("terminals", "clients", "agents", "requests", "attention"):
        assert snapshot[name] == "0", snapshot
    return snapshot


def run(config, operator_alias, observer_alias):
    os.environ["TERM"] = "xterm-256color"
    ssh = pinned_ssh(config)
    operator, observer = [*ssh, operator_alias], [*ssh, observer_alias]
    boot = require_disposable_guest(operator, observer)
    home = remote(operator, 'printf "%s\\n" "$HOME"')
    corvid = f"{home}/.local/bin/corvid"
    schema = Path(__file__).resolve().parents[3] / "crates/corvid-protocol/proto/corvid.proto"
    before = require_empty(operator, corvid)
    request_id = str(uuid.uuid4())
    command = [
        corvid, "attach", "--new-workspace", "Reboot qualification",
        "--agent", "claude", "--", "--reboot-probe",
    ]

    with OuterTerminal([*operator, "-tt", shlex.join(command)]) as terminal:
        terminal.expect(b"REBOOT-READY")
        agents = control(operator, corvid, "agents")
        run_line, = agents.splitlines()
        run_id = run_line.split()[0]
        agent = fields(run_line)
        assert agent["process"] == "Running" and agent["badge"] != "Ended", agent
        binding = register(operator, corvid, terminal, before)
        payloads = focus_payloads(schema, before, binding, run_id, request_id)
        applied = replay_focus(operator, corvid, schema, payloads, before, request_id)
        assert "focus_applied {" in applied, applied
        assert read_field(applied, "terminal_id") == agent["terminal"], applied
        assert read_field(applied, "client_id") == binding["client_id"], applied
        wait_for_save(operator, corvid)
        layout = control(operator, corvid, "workspaces")
        assert terminal.process.poll() is None, "original session ended before reboot"

        print("Original focus applied through the SSH relay. Rebooting the disposable guest.", flush=True)
        new_boot = reboot_guest(operator, observer, corvid, boot)
        terminal.process.wait(timeout=5)

    after = require_empty(operator, corvid)
    assert after["host_id"] == before["host_id"] and after["server_epoch"] != before["server_epoch"]
    assert after["sequence"] == "0", after
    assert control(operator, corvid, "workspaces") == layout
    archived, = [
        fields(line) for line in control(operator, corvid, "history").splitlines()
        if fields(line).get("run") == run_id
    ]
    assert archived["epoch"] == before["server_epoch"] and archived["terminal"] == agent["terminal"]
    assert archived["state"] == "Historical" and archived["process"] == "Unknown", archived

    stale = replay_focus(operator, corvid, schema, payloads, after, request_id)
    assert "error {" in stale and read_field(stale, "code") == "EpochMismatch", stale
    assert require_empty(operator, corvid) == after, "old focus changed the new daemon snapshot"
    print(f"Boot ID changed from {boot} to {new_boot}. Old focus returned EpochMismatch.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ssh_config", help="OpenSSH configuration with pinned guest host keys")
    parser.add_argument("operator", help="SSH alias for the dedicated Corvid account")
    parser.add_argument("observer", help="SSH alias with noninteractive sudo inside the guest")
    parser.add_argument("--reboot-disposable-vm", action="store_true", required=True)
    args = parser.parse_args()
    run(args.ssh_config, args.operator, args.observer)
