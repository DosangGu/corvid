"""Exercise Corvid in an isolated loopback SSH server and transient user service."""

import os
from pathlib import Path
import pwd
import shlex
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid

from interactive import OuterTerminal, stop


def key(path):
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", path], check=True)
    return path.with_suffix(".pub")


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def start_ssh(directory):
    account = pwd.getpwuid(os.getuid()).pw_name
    host_key = directory / "host"
    client_key = directory / "client"
    host_pub = key(host_key)
    authorized = key(client_key)
    listen = port()

    config = directory / "sshd_config"
    config.write_text(
        f"""Port {listen}
ListenAddress 127.0.0.1
HostKey {host_key}
AuthorizedKeysFile {authorized}
PidFile {directory / "sshd.pid"}
AllowUsers {account}
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
LogLevel ERROR
"""
    )
    known_hosts = directory / "known_hosts"
    public = host_pub.read_text().strip()
    known_hosts.write_text(f"[127.0.0.1]:{listen} {public}\n")
    server = subprocess.Popen(["/usr/bin/sshd", "-D", "-e", "-f", config])
    ssh = [
        "ssh", "-F", "/dev/null", "-p", str(listen), "-i", str(client_key),
        "-o", "BatchMode=yes", "-o", "ConnectTimeout=2", "-o", "IdentitiesOnly=yes",
        "-o", "ForwardAgent=no", "-o", "StrictHostKeyChecking=yes",
        "-o", "GlobalKnownHostsFile=/dev/null", "-o", f"UserKnownHostsFile={known_hosts}",
        f"{account}@127.0.0.1",
    ]
    return server, ssh


def check_ssh(server, ssh):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        assert server.poll() is None, "loopback sshd exited"
        probe = subprocess.run([*ssh, "-T", "true"], capture_output=True, timeout=3)
        if probe.returncode == 0:
            return
        time.sleep(0.05)
    raise AssertionError(probe.stderr.decode(errors="replace"))


def transcode(schema, operation, payload):
    return subprocess.run(
        ["protoc", "-I", schema.parent, f"--{operation}=corvid.protocol.v1.Envelope", schema.name],
        input=payload, capture_output=True, check=True, timeout=3,
    ).stdout


def check_control(ssh, command, schema):
    identity = subprocess.check_output([*command, "control", "host"], text=True)
    fields = dict(line.split("=", 1) for line in identity.splitlines())
    greeting = (
        'protocol_major: 1 request_id: "hello" '
        'hello { controller_id: "ssh-fixture" supported_major_versions: 1 }'
    )
    request = (
        f'protocol_major: 1 request_id: "host" host_id: "{fields["host_id"]}" '
        f'server_epoch: "{fields["server_epoch"]}" get_host {{}}'
    )
    frames = [b"CRV1"]
    for message in (greeting, request):
        payload = transcode(schema, "encode", message.encode())
        frames.append(struct.pack(">IB", len(payload) + 1, 1) + payload)
    result = subprocess.run(
        [*ssh, "-T", shlex.join([*command, "control", "--stdio"])],
        input=b"".join(frames), capture_output=True, timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout[:4] == b"CRV1", result.stdout[:16]
    replies = []
    remaining = result.stdout[4:]
    while remaining:
        length, kind = struct.unpack(">IB", remaining[:5])
        assert kind == 1 and 1 <= length <= 256 * 1024 and len(remaining) >= length + 4
        replies.append(transcode(schema, "decode", remaining[5:4 + length]))
        remaining = remaining[4 + length:]
    assert len(replies) == 2 and b"hello_result {" in replies[0] and b"host_info {" in replies[1]
    assert fields["host_id"].encode() in replies[1]
    assert fields["server_epoch"].encode() in replies[1]


def progress_during_disconnect(progress):
    before = len(progress.read_text().splitlines())
    time.sleep(0.35)
    assert len(progress.read_text().splitlines()) > before


def check_working_ssh(ssh, command, directory):
    fixture = Path(__file__).with_name("interactive.py").resolve()
    progress = directory / "progress"
    attach = [*command, "attach"]
    start = [*attach, "--", sys.executable, str(fixture), "--child", str(progress)]
    with OuterTerminal([*ssh, "-tt", shlex.join(start)]) as first:
        ready = first.expect(rb"PID=(\d+) TICK=\d+ SIZE=80x24")
        pid = ready[1]
        first.send(b"a")
        first.expect("한글 e\u0301".encode())
        first.send(b"\x1dd")
        first.wait(0)
    progress_during_disconnect(progress)

    remote = [*ssh, "-tt", shlex.join(attach)]
    with OuterTerminal(remote) as second:
        assert second.expect(rb"PID=(\d+) TICK=\d+ SIZE=80x24")[1] == pid
        second.process.kill()
        second.process.wait(timeout=3)
    progress_during_disconnect(progress)

    with OuterTerminal(remote) as third:
        assert third.expect(rb"PID=(\d+) TICK=\d+ SIZE=80x24")[1] == pid
        third.expect("한글 e\u0301".encode())
        third.send(b"q")
        third.expect(b"TERMINAL-EXIT")
        third.wait(7)


def unit_pid(unit):
    return int(subprocess.check_output(
        ["systemctl", "--user", "show", unit, "-p", "MainPID", "--value"],
        text=True,
    ).strip())


def wait_for_unit(command):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        result = subprocess.run([*command, "control", "ping"], capture_output=True, timeout=2)
        if result.returncode == 0:
            return
        time.sleep(0.05)
    raise AssertionError("Corvid user service did not become ready")


def run(binary):
    os.environ["TERM"] = "xterm-256color"
    root = Path(__file__).resolve().parents[3]
    schema = root / "crates/corvid-protocol/proto/corvid.proto"
    with tempfile.TemporaryDirectory(prefix="corvid-ssh-", dir=root) as temporary:
        directory = Path(temporary)
        database = directory / "terminfo"
        subprocess.run(["tic", "-x", "-o", database, root / "spikes/terminal/corvid.terminfo"], check=True)
        command = [str(Path(binary).resolve()), "--runtime-dir", str(directory / "runtime")]
        unit = f"corvid-fixture-{uuid.uuid4().hex}.service"
        subprocess.run(
            ["systemd-run", "--user", "--quiet", "--collect", f"--unit={unit}",
             "--property=Type=exec", "--property=KillMode=control-group",
             f"--setenv=TERMINFO={database}", *command, "daemon", "--state-dir", directory / "state"],
            check=True, timeout=10,
        )
        try:
            wait_for_unit(command)
            pid = unit_pid(unit)
            assert pid > 0
            server, ssh = start_ssh(directory)
            try:
                check_ssh(server, ssh)
                check_control(ssh, command, schema)
                check_working_ssh(ssh, command, directory)
                assert unit_pid(unit) == pid
            finally:
                stop(server)
        finally:
            subprocess.run(["systemctl", "--user", "stop", unit], check=True, timeout=12)
    print("OpenSSH control and work channels verified clean detach, abrupt loss, and same-PTY reattach.")


if __name__ == "__main__":
    run(sys.argv[1])
