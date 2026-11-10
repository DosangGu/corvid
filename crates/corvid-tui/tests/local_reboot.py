"""Qualify recovery after rebooting the controller PC while a remote question waits."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time


WINDOWS = os.name == "nt"


def read_command(command):
    return subprocess.run(
        command, encoding="utf-8", errors="replace", capture_output=True, check=True, timeout=15,
    ).stdout


def fields(line):
    return dict(field.split("=", 1) for field in line.split() if "=" in field)


def powershell(script):
    return read_command(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script])


def local_boot():
    if WINDOWS:
        return json.loads(powershell(
            "$os = Get-CimInstance Win32_OperatingSystem; "
            "@{machine=$os.CSName; version=$os.Version; build=$os.BuildNumber; "
            "boot=$os.LastBootUpTime.ToUniversalTime().Ticks.ToString()} | ConvertTo-Json -Compress"
        ))
    return {
        "machine": os.uname().nodename,
        "version": os.uname().release,
        "boot": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
    }


def wezterm_processes():
    output = powershell(
        "Get-Process -Name wezterm-gui -ErrorAction SilentlyContinue | "
        "ForEach-Object { $_.Id }; exit 0"
    )
    return {int(pid) for pid in output.splitlines() if pid}


def wezterm_panes(binding):
    assert "WEZTERM_UNIX_SOCKET" in os.environ, "run from a shell in the paired WezTerm GUI"
    panes = json.loads(read_command([
        "wezterm", "--skip-config", "cli", "--no-auto-start", "list", "--format", "json",
    ]))
    locations = {(pane["window_id"], pane["tab_id"], pane["pane_id"]) for pane in panes}
    target = tuple(int(binding["wezterm_" + key]) for key in ("window", "tab", "pane"))
    assert int(binding["wezterm_pid"]) in wezterm_processes() and target in locations, binding
    return locations


class Fixture:
    def __init__(self, args):
        self.command = [args.corvid, "companion"]
        if WINDOWS:
            self.command += ["--pipe-instance", args.pipe_instance]
            ssh = Path(os.environ["SystemRoot"]) / "System32/OpenSSH/ssh.exe"
        else:
            if args.socket_dir is not None:
                self.command += ["--socket-dir", args.socket_dir]
            ssh = "ssh"
        self.ssh = [
            ssh, "-T", "-oBatchMode=yes", "-oStrictHostKeyChecking=yes", "-oConnectTimeout=5",
            "-oClearAllForwardings=yes", "-oControlPath=none", "-oRemoteCommand=none", args.ssh_alias,
        ]

    def remote(self, command):
        return read_command([*self.ssh, command])

    def view(self, slot_id, *, detached=False):
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            header, *rows = read_command([*self.command, "slots"]).splitlines()
            status = fields(header)
            bindings = [fields(row) for row in rows if row.startswith("binding ")]
            if status["connection"] == "Ready" and (not detached or not bindings):
                slot, = [fields(row) for row in rows if row.startswith(f"slot={slot_id} ")]
                assert slot["availability"] == "Available" and slot["pending"] == "1", slot
                return status, slot, bindings
            time.sleep(0.25)
        raise AssertionError((status, bindings))

    def remote_work(self, slot):
        run = slot["run"].rsplit("/", 1)[1]
        daemon = fields(self.remote("corvid control snapshot"))
        agent, = [
            fields(row) for row in self.remote("corvid control agents").splitlines()
            if row.split()[0] == run
        ]
        question, = [
            row for row in map(fields, self.remote("corvid control attention").splitlines())
            if row["run_id"] == run and row["request_kind"] == "Question"
            and row.get("request_status") == "Pending"
        ]
        assert agent["process"] == "Running" and agent["pending"] == "1", agent
        return {
            "remote_boot": self.remote("cat /proc/sys/kernel/random/boot_id").strip(),
            "daemon": {key: daemon[key] for key in
                       ("host_id", "server_epoch", "terminals", "agents", "workspaces",
                        "requests", "attention")},
            "run": run,
            "pid": agent["pid"],
            "terminal": agent["terminal"],
            "question": question,
        }

    def clients(self):
        rows = self.remote("corvid control clients").splitlines()
        return {row.split()[0]: fields(row) for row in rows}

    def focus(self, slot, capture, *, fault=None):
        result = subprocess.run(
            [*self.command, "focus-slot", "--slot", slot["capture"], "--binding", capture],
            text=True, capture_output=True, timeout=15,
        )
        if fault is not None:
            assert result.returncode != 0 and f"{fault}:" in result.stderr, result
            return
        assert result.returncode == 0, result.stderr
        return fields(result.stdout)


def capture_before(fixture, path, slot_id):
    assert not path.exists(), f"checkpoint already exists: {path}"
    header, slot, bindings = fixture.view(slot_id)
    assert slot["pinned"] == "true", "pin the assigned slot to verify persisted placement"
    binding, = bindings
    work = fixture.remote_work(slot)
    (client, state), = fixture.clients().items()
    assert client == binding["client_id"] and state["terminal"] == work["terminal"], state
    if WINDOWS:
        wezterm_panes(binding)
    boot = local_boot()
    assert boot["boot"] != work["remote_boot"], "use a separate execution host"
    with path.open("x", encoding="utf-8") as file:
        json.dump({
            "before_boot": boot, "header": header, "slot": slot, "binding": binding, "work": work,
        }, file, indent=2)
    print(f"Captured pending question in slot {slot_id}. Reboot the controller PC manually.")


def verify_restored(fixture, checkpoint, *, detached):
    header, slot, bindings = fixture.view(checkpoint["slot"]["slot"], detached=detached)
    previous = checkpoint["header"]
    for key in ("host_id", "controller_id", "remote_epoch"):
        assert header[key] == previous[key], (key, previous, header)
    assert header["companion_epoch"] != previous["companion_epoch"], header
    for key in ("slot", "run", "revision", "pinned"):
        assert slot[key] == checkpoint["slot"][key], slot
    work = fixture.remote_work(slot)
    assert work == checkpoint["work"], (checkpoint["work"], work)
    return header, slot, bindings


def old_client_capture(header, checkpoint):
    binding = checkpoint["binding"]
    return f"{header['control_capture']}/{binding['client_id']}/{binding['revision']}"


def verify_detached(fixture, path, checkpoint):
    boot = local_boot()
    assert boot["machine"] == checkpoint["before_boot"]["machine"], boot
    assert boot["boot"] != checkpoint["before_boot"]["boot"], "controller PC has not rebooted"
    if WINDOWS:
        assert not wezterm_processes(), "run recovery before opening WezTerm"
    header, slot, bindings = verify_restored(fixture, checkpoint, detached=True)
    assert not bindings and not fixture.clients(), "old working client or binding remains"
    fixture.focus(checkpoint["slot"], checkpoint["binding"]["capture"], fault="StaleBinding")
    fixture.focus(slot, old_client_capture(header, checkpoint), fault="NoAttachedClient")
    assert not fixture.clients() and fixture.remote_work(slot) == checkpoint["work"]
    if WINDOWS:
        assert not wezterm_processes(), "rejected focus created a WezTerm GUI"
    checkpoint["recovered_boot"] = boot
    checkpoint["recovered_epoch"] = header["companion_epoch"]
    temporary = path.with_suffix(path.suffix + ".new")
    with temporary.open("x", encoding="utf-8") as file:
        json.dump(checkpoint, file, indent=2)
    temporary.replace(path)
    print("Slots recovered with no attached TUI. Open SSH and pair the new TUI manually.")


def verify_repaired(fixture, checkpoint):
    assert local_boot() == checkpoint.get("recovered_boot"), "verify detached recovery first"
    header, slot, bindings = verify_restored(fixture, checkpoint, detached=False)
    assert header["companion_epoch"] == checkpoint["recovered_epoch"], header
    binding, = bindings
    client = binding["client_id"]
    assert client != checkpoint["binding"]["client_id"], binding
    before = fixture.clients()
    assert set(before) == {client}, before
    assert before[client]["terminal"] != checkpoint["work"]["terminal"], "select another Corvid tab first"
    if WINDOWS:
        processes = wezterm_processes()
        panes = wezterm_panes(binding)

    fixture.focus(slot, old_client_capture(header, checkpoint), fault="StaleBinding")
    assert fixture.clients() == before, "stale focus changed the working TUI"
    result = fixture.focus(slot, binding["capture"])
    assert result["remote_selection"] == "Applied" and result["client_id"] == client, result
    assert result["run_id"] == checkpoint["work"]["run"], result
    assert result["terminal_id"] == checkpoint["work"]["terminal"], result
    after = fixture.clients()
    assert set(after) == {client} and after[client]["terminal"] == result["terminal_id"], after
    assert after[client]["view_revision"] == result["view_revision"], after
    assert fixture.remote_work(slot) == checkpoint["work"], "focus changed the remote question"
    if WINDOWS:
        assert result["local_activation"] in {"Activated", "PaneSelectedForegroundUnknown"}, result
        for key in ("wezterm_pid", "wezterm_window", "wezterm_tab", "wezterm_pane"):
            assert result[key] == binding[key], result
        assert wezterm_processes() == processes and wezterm_panes(binding) == panes
    print(f"New registration focused the original pending question ({result['local_activation']}).")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("before", "recovered", "repaired"))
    parser.add_argument("checkpoint", type=Path)
    parser.add_argument("--ssh-alias", required=True)
    parser.add_argument("--corvid", default="corvid.exe" if WINDOWS else "corvid")
    parser.add_argument("--slot", type=int, default=1, help="assigned slot to verify before reboot")
    if WINDOWS:
        parser.add_argument("--pipe-instance", default="main")
    else:
        parser.add_argument("--socket-dir", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"(?!-)[a-zA-Z0-9_.-]{1,253}", args.ssh_alias):
        parser.error("use a configured OpenSSH alias")
    fixture = Fixture(args)
    if args.phase == "before":
        capture_before(fixture, args.checkpoint, args.slot)
    else:
        checkpoint = json.loads(args.checkpoint.read_text(encoding="utf-8"))
        if args.phase == "recovered":
            verify_detached(fixture, args.checkpoint, checkpoint)
        else:
            verify_repaired(fixture, checkpoint)


if __name__ == "__main__":
    main()
