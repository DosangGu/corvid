#!/usr/bin/env python3
"""Run the Desktop title regression through a disposable Linux loopback SSH server."""

import argparse
import os
from pathlib import Path
import pwd
import shlex
import shutil
import signal
import socket
import subprocess
import tempfile
import time


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def stop(process):
    if process is None or process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def verify(binary, root, cargo_options):
    sshd = shutil.which("sshd") or "/usr/sbin/sshd"
    ssh = shutil.which("ssh")
    if not ssh or not Path(sshd).is_file():
        raise RuntimeError("Install OpenSSH client/server before running this fixture")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    for name in ("host", "client"):
        run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / name))
    (root / "authorized_keys").write_bytes((root / "client.pub").read_bytes())
    key = " ".join((root / "host.pub").read_text().split()[:2])
    (root / "known_hosts").write_text(f"[127.0.0.1]:{port} {key}\n")
    runtime = root / "r"
    environment = os.environ | {"HOME": str(root / "home"), "CORVID_RUNTIME_DIR": str(runtime)}
    (root / "home").mkdir(mode=0o700)
    (root / "bin").mkdir(mode=0o700)
    remote = root / "remote.py"
    remote.write_text(
        "import os\n"
        "commands = {'corvid terminal-relay': ['terminal-relay'], "
        "'corvid control --stdio': ['control', '--stdio']}\n"
        f"binary = {str(binary)!r}\n"
        f"runtime = {str(runtime)!r}\n"
        "args = commands[os.environ['SSH_ORIGINAL_COMMAND']]\n"
        "os.execv(binary, [binary, '--runtime-dir', runtime, *args])\n"
    )
    config = root / "sshd_config"
    config.write_text(
        f"AddressFamily inet\nListenAddress 127.0.0.1\nPort {port}\n"
        f"HostKey {root / 'host'}\nPidFile {root / 'sshd.pid'}\n"
        f"AuthorizedKeysFile {root / 'authorized_keys'}\n"
        "AuthenticationMethods publickey\nPasswordAuthentication no\n"
        "KbdInteractiveAuthentication no\nUsePAM no\nPermitRootLogin no\n"
        "AllowTcpForwarding no\nX11Forwarding no\nLogLevel ERROR\n"
        f"ForceCommand /usr/bin/python3 {shlex.quote(str(remote))}\n"
    )
    client_config = root / "ssh_config"
    client_config.write_text(
        "Host corvid-title-test\n"
        f" HostName 127.0.0.1\n Port {port}\n User {pwd.getpwuid(os.getuid()).pw_name}\n"
        f" IdentityFile {root / 'client'}\n IdentitiesOnly yes\n"
        f" UserKnownHostsFile {root / 'known_hosts'}\n GlobalKnownHostsFile /dev/null\n"
        " BatchMode yes\n StrictHostKeyChecking yes\n"
    )
    wrapper = root / "bin/ssh"
    wrapper.write_text(
        f"#!/bin/sh\nexec {shlex.quote(ssh)} "
        f"-F {shlex.quote(str(client_config))} \"$@\"\n"
    )
    wrapper.chmod(0o700)
    run(sshd, "-t", "-f", str(config))
    daemon = server = None
    try:
        server = subprocess.Popen(
            [sshd, "-D", "-e", "-f", str(config)], start_new_session=True
        )
        daemon = subprocess.Popen(
            [
                str(binary), "--runtime-dir", str(runtime), "daemon",
                "--state-dir", str(root / "state"), "--autostarted",
            ],
            env=environment,
            start_new_session=True,
        )
        for _ in range(100):
            if daemon.poll() is not None or server.poll() is not None:
                raise RuntimeError("Fixture daemon or SSH server exited during startup")
            probe = subprocess.run(
                [str(binary), "--runtime-dir", str(runtime), "control", "ping"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=6,
            )
            if probe.returncode == 0:
                break
            time.sleep(0.05)
        else:
            raise RuntimeError("Fixture daemon did not start")
        run(
            "cargo", "test", "--locked", "--offline", *cargo_options,
            "-p", "corvid-desktop",
            "terminal_session::inventory_watch::terminal_tests::ssh_pty_titles_reach_desktop_and_survive_reattach",
            "--", "--ignored", "--exact", "--nocapture",
            env=os.environ | {
                "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
                "CORVID_TITLE_TEST_SSH_ALIAS": "corvid-title-test",
            },
        )
    finally:
        stop(daemon)
        stop(server)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary", type=Path, required=True, help="Matching Linux corvid executable"
    )
    parser.add_argument("--target", help="Cargo target used to build the executable")
    parser.add_argument("--release", action="store_true", help="Use release test binaries")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    cargo_options = ["--target", args.target] if args.target else []
    if args.release:
        cargo_options.append("--release")
    # Short private paths leave room for Unix and terminal-host socket names.
    with tempfile.TemporaryDirectory(prefix="cv-title-", dir=Path.home()) as temporary:
        verify(binary, Path(temporary), cargo_options)


if __name__ == "__main__":
    main()
