"""Check the deployed user unit across the last SSH login on a dedicated Linux host."""

import argparse
import os
from pathlib import Path
import shlex
import subprocess
import time

from interactive import OuterTerminal


def remote(ssh, command, *, data=None):
    return subprocess.run(
        [*ssh, "-T", command], input=data, stdout=subprocess.PIPE, encoding="utf-8",
        check=True, timeout=8,
    ).stdout.strip()


def user_status(observer, uid):
    output = remote(observer, f"loginctl show-user {uid} --all -p Linger -p Sessions -p State")
    return dict(line.split("=", 1) for line in output.splitlines())


def login_sessions(observer, status):
    sessions = []
    for session in status["Sessions"].split():
        command = [
            "loginctl", "show-session", session, "-p", "Class", "-p", "Remote", "-p", "Service",
        ]
        try:
            output = remote(observer, shlex.join(command))
        except subprocess.CalledProcessError:
            # A session can disappear between the two logind queries.
            return None
        properties = dict(line.split("=", 1) for line in output.splitlines())
        if properties["Class"] not in {"manager", "manager-early"}:
            sessions.append(properties)
    return sessions


def wait_for_logout(observer, uid):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        status = user_status(observer, uid)
        assert status["Linger"] == "yes", "enable lingering for the dedicated account first"
        if status["State"] == "lingering" and login_sessions(observer, status) == []:
            return
        time.sleep(0.1)
    raise AssertionError(f"last login did not close: {status}")


def check_sole_ssh_login(observer, uid):
    sessions = login_sessions(observer, user_status(observer, uid))
    assert sessions == [{"Class": "user", "Remote": "yes", "Service": "sshd"}], sessions


def incarnation(observer, pid):
    stat = remote(observer, f"sudo -n cat /proc/{pid}/stat")
    # The command name is parenthesized and may itself contain spaces or parentheses.
    return pid, stat.rsplit(")", 1)[1].split()[19]


def runtime_identity(observer, user_command, corvid, child):
    daemon_pid = int(remote(observer, shlex.join([
        *user_command, "systemctl", "--user", "show", "corvid.service", "-p", "MainPID", "--value",
    ])))
    assert daemon_pid > 0, "corvid.service lost its daemon"
    host = remote(observer, shlex.join([*user_command, corvid, "control", "host"]))
    tty = remote(observer, f"sudo -n readlink /proc/{child}/fd/0")
    assert tty.startswith("/dev/pts/"), tty
    device = remote(observer, f"sudo -n stat -Lc '%d:%i:%t:%T' /proc/{child}/fd/0")
    return incarnation(observer, daemon_pid), incarnation(observer, child), host, tty, device


def check_disconnected(observer, uid, progress, identity, expected):
    wait_for_logout(observer, uid)
    counter = shlex.join(["sudo", "-n", "tail", "-n", "1", progress])
    before = int(remote(observer, counter).split()[0])
    assert identity() == expected, "daemon, process, or PTY changed after logout"
    time.sleep(0.35)
    assert int(remote(observer, counter).split()[0]) > before, "PTY child stopped progressing"
    wait_for_logout(observer, uid)


def run(config, operator_alias, observer_alias):
    os.environ["TERM"] = "xterm-256color"
    ssh = [
        "ssh", "-F", config, "-o", "BatchMode=yes", "-o", "ControlPath=none",
        "-o", "StrictHostKeyChecking=yes", "-o", "ClearAllForwardings=yes", "-o", "ForwardAgent=no",
    ]
    operator = [*ssh, operator_alias]
    observer = [*ssh, observer_alias]
    system = "id -u; cat /proc/sys/kernel/random/boot_id"
    uid, boot, gid, home = remote(operator, system + '; id -g; printf "%s\\n" "$HOME"').splitlines()
    observer_uid, observer_boot = remote(observer, system).splitlines()
    assert uid.isdecimal() and gid.isdecimal(), (uid, gid)
    assert observer_uid != uid and observer_boot == boot, "use two accounts on the same host"

    fixture = Path(__file__).with_name("interactive.py")
    directory = remote(operator, 'mktemp -d "$HOME/.corvid-last-login.XXXXXXXX"')
    child_path = f"{directory}/interactive.py"
    progress = f"{directory}/progress"
    remote(
        operator, shlex.join(["tee", child_path]) + " >/dev/null",
        data=fixture.read_text(encoding="utf-8"),
    )
    wait_for_logout(observer, uid)

    runtime = f"/run/user/{uid}"
    user_command = [
        "sudo", "-n", "setpriv", "--reuid", uid, "--regid", gid, "--init-groups", "env",
        f"HOME={home}", f"XDG_RUNTIME_DIR={runtime}",
        f"DBUS_SESSION_BUS_ADDRESS=unix:path={runtime}/bus",
    ]
    corvid = f"{home}/.local/bin/corvid"
    attach = [*operator, "-tt"]
    screen = rb"PID=(\d+) TICK=\d+ SIZE=80x24"
    marker = "한글 e\u0301".encode()
    start = [
        corvid, "attach", "--new", "--", "timeout", "--foreground", "120s",
        "python3", child_path, "--child", progress,
    ]
    with OuterTerminal([*attach, shlex.join(start)]) as first:
        child = int(first.expect(screen)[1])
        check_sole_ssh_login(observer, uid)
        first.send(b"a")
        first.expect(marker)
        expected = runtime_identity(observer, user_command, corvid, child)
        first.send(b"\x1dd")
        first.wait(0)

    def identity():
        return runtime_identity(observer, user_command, corvid, child)

    check_disconnected(observer, uid, progress, identity, expected)
    print("Clean detach closed the sole SSH login. The daemon and PTY child stayed active.", flush=True)

    reattach = [*attach, shlex.join([corvid, "attach"])]
    with OuterTerminal(reattach) as second:
        assert int(second.expect(screen)[1]) == child
        second.expect(marker)
        check_sole_ssh_login(observer, uid)
        second.process.kill()
        second.process.wait(timeout=3)

    check_disconnected(observer, uid, progress, identity, expected)
    with OuterTerminal(reattach) as third:
        assert int(third.expect(screen)[1]) == child
        third.expect(marker)
        third.send(b"q")
        third.expect(b"TERMINAL-EXIT")
        third.wait(7)
    print("Forced SSH loss closed the sole login. Reattach restored the same PTY and screen.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ssh_config", help="OpenSSH configuration with pinned host keys")
    parser.add_argument("operator", help="SSH alias for the dedicated Corvid account")
    parser.add_argument("observer", help="SSH alias for the separate account with noninteractive sudo")
    args = parser.parse_args()
    run(args.ssh_config, args.operator, args.observer)
