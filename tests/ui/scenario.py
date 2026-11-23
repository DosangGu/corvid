"""Desktop and Deck scenarios driven through native window and keyboard events."""
from contextlib import ExitStack, contextmanager
import os
import shlex
import subprocess
import time
from uuid import uuid4

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
    # Detached Windows subprocesses pin their current directory until they exit.
    cwd = root.parent if os.name == "nt" else root
    process = subprocess.Popen(command, env=env, cwd=cwd, stdout=log, stderr=log)
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


def verify_paste(ui, desktop, window, root):
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

    pasted = "한글 e\u0301 🐦"
    if os.name != "nt":
        # TODO: Stabilize macOS tab navigation in UI qualification
        # Synchronize tab selection and shell readiness before extending this smoke case.
        # Issue URL: https://github.com/DosangGu/corvid/issues/357
        command("pasted", mark("pasted", shlex.quote(pasted)), pasted, probe=True)
        return

    assignment = "$tag='first'; "
    tag = "$tag"
    command("first", mark("first", tag, assignment=assignment), "first", probe=True)
    ui.keys(window, "leader", "t", "n")
    # The first shell already has $tag. Only a fresh tab can acknowledge this probe.
    fresh = "if ([string]::IsNullOrEmpty($tag)) { " + mark("ready", "'ready'") + " }"
    command("ready", fresh, "ready", probe=True)

    assignment = f"$tag='{pasted}'; "
    command("pasted", mark("pasted", tag, assignment=assignment), pasted)
    ui.keys(window, "leader", "t", "h", "return")
    command("isolated", mark("isolated", tag), "first", probe=True)


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
                reader, epoch = monitor_fixture.ready(cli, state, env)
                if case == "desktop-paste":
                    verify_paste(ui, app, window, root)
                elif case == "deck-window":
                    if os.name == "nt":
                        for shortcut in ui.fixture_shortcuts(state):
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
