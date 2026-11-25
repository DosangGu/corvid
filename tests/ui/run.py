#!/usr/bin/env python3
"""Run Corvid's UI tests with local builds or release assets."""
import argparse
from contextlib import ExitStack, contextmanager
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DEPLOY = ROOT / "deploy"
sys.path.insert(0, str(DEPLOY))

SMOKE = {
    "Linux": ("desktop-paste", "deck-window"),
    "Windows": ("desktop-window", "deck-window"),
    "Darwin": ("desktop-paste", "deck-window"),
}
CASES = {
    "Linux": (*SMOKE["Linux"], "desktop-lifecycle", "deck-focus"),
    "Windows": (*SMOKE["Windows"], "desktop-paste", "deck-tray"),
    "Darwin": (*SMOKE["Darwin"], "deck-focus", "dmg-windows"),
}


def linux(case, build, assets, artifacts):
    screenshot = artifacts / "failure.png"
    screenshot.unlink(missing_ok=True)
    env = dict(os.environ, CORVID_UI_FAILURE_SCREENSHOT=str(screenshot), XDG_SESSION_TYPE="x11")
    env.pop("WAYLAND_DISPLAY", None)
    if case == "deck-focus":
        args = [HERE / "linux/test_deck_focus.py", "--build", build,
                "--failure-screenshot", screenshot]
        for variable, flag in [
            ("CORVID_DECK_HISTORY_SCREENSHOT", "--history-screenshot"),
            ("CORVID_DECK_TOOLTIP_SCREENSHOT", "--tooltip-screenshot"),
        ]:
            if destination := env.get(variable):
                args += [flag, destination]
    else:
        args = [HERE / "linux/test_desktop.py", "--build", build] if build else [
            HERE / "linux/test_desktop.py", assets,
        ]
        if case == "desktop-paste":
            args += ["--paste-only"]
        elif case == "deck-window":
            args += ["--deck-only"]
    subprocess.run(["xvfb-run", "-a", "-s", "-screen 0 1280x1024x24", sys.executable, *args], env=env, check=True)


def unique_asset(directory, pattern):
    matches = list(directory.glob(pattern))
    if len(matches) != 1:
        raise ValueError(f"Expected one {pattern} in {directory}, found {len(matches)}")
    return matches[0]


def windows_machine():
    import winreg

    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE,
                        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment") as key:
        architecture, _ = winreg.QueryValueEx(key, "PROCESSOR_ARCHITECTURE")
    return {"AMD64": "x86_64", "ARM64": "aarch64"}[architecture.upper()]


def binaries(stack, system, build, assets, root):
    suffix = ".exe" if system == "Windows" else ""
    extracted = root / "package"
    if build:
        payload = deck = build
    elif system == "Windows":
        machine = windows_machine()
        archive = unique_asset(assets, f"corvid-desktop-*-{machine}-pc-windows-msvc.zip")
        with zipfile.ZipFile(archive) as stream:
            stream.extractall(extracted)
        payload = extracted
        deck = extracted / "plugins/agent-deck"
    else:
        from macos.test_dmg_windows import attach_dmg, run
        dmg = unique_asset(assets, "corvid-desktop-*-aarch64-apple-darwin.dmg")
        attach_dmg(dmg, extracted)
        stack.callback(run, "hdiutil", "detach", extracted)
        app = extracted / "Corvid.app/Contents"
        payload = app / "MacOS"
        deck = app / "Helpers/Corvid Agent Deck.app/Contents/MacOS"
    return (payload / f"corvid{suffix}", payload / f"corvid-desktop{suffix}",
            deck / f"corvid-agent-deck{suffix}")


@contextmanager
def fixture_directory(parent):
    directory = tempfile.TemporaryDirectory(prefix="cvui-", dir=parent)
    try:
        yield Path(directory.name)
    finally:
        deadline = time.monotonic() + 8
        while True:
            try:
                directory.cleanup()
                break
            except PermissionError:
                # A stopped Windows monitor can hold an executable briefly after its pipe closes.
                if os.name != "nt" or time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)


def native(system, cases, build, assets, artifacts):
    from scenario import run_case

    if system == "Darwin":
        from macos.driver import Desktop, bundle_deck, bundle_desktop
        from macos.test_desktop_paste import verify_paste
    else:
        from windows.driver import Desktop
        from windows.test_desktop_paste import verify_paste

    # Corvid checks Unix socket directory ownership and permissions.
    parent = Path.home() if system == "Darwin" else None
    for case in cases:
        print(f"RUN {case}", flush=True)
        if case == "dmg-windows":
            from macos.test_dmg_windows import check
            check(assets)
            continue
        with ExitStack() as stack:
            root = stack.enter_context(fixture_directory(parent))
            paths = binaries(stack, system, build, assets, root)
            if build and system == "Darwin" and case in ("deck-window", "deck-focus"):
                cli, desktop, deck = paths
                if case == "deck-focus":
                    cli, desktop = bundle_desktop(cli, desktop, root)
                paths = (cli, desktop, bundle_deck(deck, root))
            if case == "deck-tray":
                env = dict(os.environ, CORVID_UI_DECK_BINARY=str(paths[2]))
                subprocess.run(["cargo", "test", "--locked", "--release", "-p", "corvid-agent-deck-ui",
                                "--test", "windows_tray", "--", "--ignored", "--nocapture"],
                               cwd=ROOT, env=env, check=True)
                continue
            ui = Desktop(root) if system == "Darwin" else Desktop()
            run_case(case, root, artifacts / case, paths, ui, verify_paste)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--build", type=Path, help="directory containing built executables")
    source.add_argument("--assets", type=Path, help="directory containing release packages")
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--suite", choices=["smoke", "full"], default="smoke")
    selection.add_argument("--case", help="run one scenario on the current platform")
    selection.add_argument("--list", action="store_true", help="list cases on the current platform")
    parser.add_argument("--artifacts", type=Path, default=ROOT / "target/ui-tests")
    args = parser.parse_args()
    system = platform.system()
    if system not in CASES:
        parser.error(f"unsupported platform {system}")
    if args.list:
        print(*CASES[system], sep="\n")
        return
    if not (args.build or args.assets):
        parser.error("provide --build or --assets")
    if args.case and args.case not in CASES[system]:
        parser.error(f"unknown case for {system}: {args.case}; available: {', '.join(CASES[system])}")
    if args.case == "deck-focus" and system == "Linux" and not args.build:
        parser.error("deck-focus requires --build")
    if args.case == "dmg-windows" and not args.assets:
        parser.error("dmg-windows requires --assets")
    if system == "Linux" and not shutil.which("xvfb-run"):
        parser.error("install Xvfb and the Linux UI dependencies listed in docs/08_ci_validation.md")
    build = args.build.resolve() if args.build else None
    assets = args.assets.resolve() if args.assets else None
    cases = (args.case,) if args.case else SMOKE[system] if args.suite == "smoke" else CASES[system]
    if not args.case and args.suite == "smoke" and system == "Windows" and windows_machine() == "x86_64":
        cases = ("desktop-paste", "deck-window")
    # The full suite expands with the checks supported by the selected input.
    if not args.case:
        cases = tuple(case for case in cases
                      if (case != "deck-focus" or system != "Linux" or build)
                      and (case != "dmg-windows" or assets))
    artifacts = args.artifacts.resolve()
    if system != "Linux":
        native(system, cases, build, assets, artifacts)
        return
    for case in cases:
        print(f"RUN {case}", flush=True)
        directory = artifacts / case
        directory.mkdir(parents=True, exist_ok=True)
        linux(case, build, assets, directory)


if __name__ == "__main__":
    main()
