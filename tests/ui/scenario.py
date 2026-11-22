"""Desktop and Deck scenarios driven through native window and keyboard events."""
from contextlib import ExitStack, contextmanager
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import time
from uuid import UUID, uuid4

import monitor_fixture


def wait_for(description, probe, process, timeout=25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if (status := process.poll()) is not None:
            raise AssertionError(f"{description}: process exited with status {status}")
        if result := probe():
            return result
        time.sleep(0.1)
    raise AssertionError(f"Timed out waiting for {description}")


@contextmanager
def running(command, env, root, log, *, children=False):
    process = subprocess.Popen(command, env=env, cwd=root, stdout=log, stderr=log)
    try:
        yield process
    finally:
        if process.poll() is None:
            if children and os.name == "nt":
                subprocess.run(["taskkill.exe", "/t", "/f", "/pid", str(process.pid)],
                               capture_output=True, timeout=10)
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)


def verify_paste(ui, cli, desktop, window, env, root):
    def terminal(previous=None, expected=None):
        def selected():
            output = subprocess.check_output([cli, "control", "clients"], env=env,
                                             encoding="utf-8", timeout=5)
            writers = [line for line in output.splitlines() if "pane-writer=" in line]
            match = len(writers) == 1 and re.search(r" terminal=(\S+)", writers[0])
            if match and match[1] != previous and (expected is None or expected == match[1]):
                return match[1]
        return wait_for("selected tab's input route", selected, desktop)

    def command(marker, script, expected, *, probe=False):
        path = root / marker
        if probe:
            # The input route can be published before the shell accepts input.
            def ready():
                if path.is_file() and path.read_text(encoding="utf-8") == expected:
                    return True
                ui.submit(window, script, clear=True)
                time.sleep(1)
            wait_for(f"keyboard acknowledgement for {marker}", ready, desktop)
        else:
            ui.submit(window, script)
            wait_for(marker, lambda: path.is_file() and path.read_text(encoding="utf-8") == expected,
                     desktop)

    def mark(marker, text, *, assignment=""):
        if os.name == "nt":
            return f"{assignment}[IO.File]::WriteAllText((Join-Path $env:CORVID_UI_ROOT '{marker}'), {text})"
        path = shlex.quote(str(root / marker))
        return f"{assignment}printf %s {text} > {path}"

    first = terminal()
    assignment = "$tag='first'; " if os.name == "nt" else "tag=first; "
    tag = "$tag" if os.name == "nt" else '"$tag"'
    command("first", mark("first", tag, assignment=assignment), "first", probe=True)
    ui.keys(window, "leader", "t", "n")
    second = terminal(previous=first)
    command("ready", mark("ready", "'ready'"), "ready", probe=True)

    pasted = "한글 e\u0301 🐦"
    assignment = f"$tag='{pasted}'; " if os.name == "nt" else f"tag={shlex.quote(pasted)}; "
    command("pasted", mark("pasted", tag, assignment=assignment), pasted)
    terminal(expected=second)
    ui.keys(window, "leader", "previous")
    terminal(expected=first)
    command("isolated", mark("isolated", tag), "first", probe=True)


def fixture_shortcuts(state):
    script = (
        "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); "
        "[Environment]::GetFolderPath('Programs')"
    )
    programs = Path(subprocess.check_output(
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
        encoding="utf-8", timeout=10).strip())
    for record in monitor_fixture.records(state):
        controller = UUID(json.loads(record.read_text(encoding="utf-8"))["controller"])
        path = programs / f"Corvid Agent Deck {controller.hex[:8]}.lnk"
        if path.exists():
            raise FileExistsError(f"Fixture shortcut already exists: {path}")
        yield path


def run_case(case, root, artifacts, paths, ui):
    cli, desktop, deck = paths
    artifacts.mkdir(parents=True, exist_ok=True)
    state = root / "c"
    local = root / "local"
    local.mkdir()
    env = dict(os.environ, HOME=str(root), USERPROFILE=str(root), LOCALAPPDATA=str(local),
               APPDATA=str(local), CORVID_RUNTIME_DIR=str(root / "r"),
               CORVID_VIEW_STATE_DIR=str(root / "v"), CORVID_UI_ROOT=str(root))
    if os.name != "nt":
        env.update(SHELL="/bin/sh", ENV="/dev/null", LC_ALL="en_US.UTF-8")
    log_path = artifacts / "process.log"
    screenshot = artifacts / "failure.png"
    screenshot.unlink(missing_ok=True)
    with log_path.open("wb") as log, running([cli, "daemon", "--state-dir", root / "d"],
                                             env, root, log, children=True) as daemon:
        wait_for("fixture daemon", lambda: subprocess.run([cli, "control", "ping"], env=env,
                 capture_output=True, timeout=5).returncode == 0, daemon)
        window = None
        with ExitStack() as processes:
            # The automatic monitor is detached from the Desktop process.
            processes.callback(monitor_fixture.stop, cli, state, env)
            try:
                command = [desktop, "--state-dir", state]
                if os.name == "nt":
                    command += ["--pipe-instance", uuid4().hex]
                app = processes.enter_context(running(command, env, root, log))
                window = wait_for("Desktop window", lambda: ui.window(app.pid), app)
                if case == "desktop-paste":
                    verify_paste(ui, cli, app, window, env, root)
                else:
                    reader, epoch = monitor_fixture.ready(cli, state, env)
                    if os.name == "nt":
                        for shortcut in fixture_shortcuts(state):
                            processes.callback(shortcut.unlink, missing_ok=True)
                    panel = processes.enter_context(running([deck, "--state-dir", state], env, root, log))
                    window = wait_for("Agent Deck window", lambda: ui.window(panel.pid), panel)
                    monitor_fixture.assert_same(reader, epoch, env)
            except Exception:
                if window:
                    try:
                        ui.screenshot(window, screenshot)
                    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                        print(f"Could not capture fixture window: {error}", flush=True)
                raise
    print(f"PASS {case}", flush=True)
