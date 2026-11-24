"""Desktop and Deck scenarios driven through native window and keyboard events."""
from contextlib import ExitStack, contextmanager
import os
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


def expect_shell_output(ui, desktop, window, marker, command, expected, *, probe=False):
    def received():
        return marker.is_file() and marker.read_text(encoding="utf-8") == expected

    if not probe:
        ui.submit(window, command)
        wait_for(marker.name, received, desktop)
        return

    # An input route can be published before the shell accepts commands.
    def ready():
        if received():
            return True
        ui.submit(window, command, clear=True)
        time.sleep(1)

    wait_for(f"keyboard acknowledgement for {marker.name}", ready, desktop)


def run_case(case, root, artifacts, paths, ui, verify_paste):
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
    if case == "deck-focus":
        from deck_focus_fixture import agent_environment, launch_agent
        from macos.test_deck_focus import verify_focus

        env = agent_environment(cli, root, env)
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
                if case == "deck-focus":
                    for name in ("first", "second"):
                        launch_agent(cli, env, root, name)
                command = [desktop, "--state-dir", state]
                if os.name == "nt":
                    command += ["--pipe-instance", uuid4().hex]
                app = processes.enter_context(running(command, env, root, log))
                window = wait_for("Desktop window", lambda: ui.window(app.pid), app)
                reader, epoch = monitor_fixture.ready(cli, state, env)
                if case == "desktop-paste":
                    verify_paste(ui, app, window, root, cli=cli, env=env)
                elif case == "deck-focus":
                    deck_command = [deck, "--state-dir", state]
                    panel = processes.enter_context(running(
                        [*deck_command, "--background"], env, root, log))
                    backdrop = processes.enter_context(running([ui.helper, "backdrop"], env, root, log))
                    verify_focus(ui, app, window, panel, backdrop, deck_command, root,
                                 cli=cli, env=env, reader=reader)
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
