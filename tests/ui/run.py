#!/usr/bin/env python3
"""Run Corvid's UI tests with local builds or release assets."""
import argparse
from contextlib import ExitStack
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import zipfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DEPLOY = ROOT / "deploy"
sys.path.insert(0, str(DEPLOY))

SMOKE = ("desktop-paste", "deck-window")
CASES = {
    "Linux": (*SMOKE, "desktop-lifecycle", "deck-focus"),
    "Windows": (*SMOKE, "deck-tray"),
    "Darwin": (*SMOKE, "dmg-windows"),
}


def linux(case, build, assets, artifacts):
    screenshot = artifacts / "failure.png"
    screenshot.unlink(missing_ok=True)
    env = dict(os.environ, CORVID_UI_FAILURE_SCREENSHOT=str(screenshot), XDG_SESSION_TYPE="x11")
    env.pop("WAYLAND_DISPLAY", None)
    if case == "deck-focus":
        args = [DEPLOY / "test_deck_focus.py", "--build", build,
                "--failure-screenshot", screenshot]
        for variable, flag in [
            ("CORVID_DECK_HISTORY_SCREENSHOT", "--history-screenshot"),
            ("CORVID_DECK_TOOLTIP_SCREENSHOT", "--tooltip-screenshot"),
        ]:
            if destination := env.get(variable):
                args += [flag, destination]
    else:
        args = [DEPLOY / "test_desktop.py", "--build", build] if build else [
            DEPLOY / "test_desktop.py", assets,
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


def binaries(stack, system, build, assets, root):
    suffix = ".exe" if system == "Windows" else ""
    extracted = root / "package"
    if build:
        payload = deck = build
    elif system == "Windows":
        import winreg
        with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE,
                            r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment") as key:
            architecture, _ = winreg.QueryValueEx(key, "PROCESSOR_ARCHITECTURE")
        machine = {"AMD64": "x86_64", "ARM64": "aarch64"}[architecture.upper()]
        archive = unique_asset(assets, f"corvid-desktop-*-{machine}-pc-windows-msvc.zip")
        with zipfile.ZipFile(archive) as stream:
            stream.extractall(extracted)
        payload = extracted
        deck = extracted / "plugins/agent-deck"
    else:
        from test_macos import attach_dmg, run
        dmg = unique_asset(assets, "corvid-desktop-*-aarch64-apple-darwin.dmg")
        attach_dmg(dmg, extracted)
        stack.callback(run, "hdiutil", "detach", extracted)
        app = extracted / "Corvid.app/Contents"
        payload = app / "MacOS"
        deck = app / "Helpers/Corvid Agent Deck.app/Contents/MacOS"
    return (payload / f"corvid{suffix}", payload / f"corvid-desktop{suffix}",
            deck / f"corvid-agent-deck{suffix}")


def native(system, cases, build, assets, artifacts):
    from scenario import run_case

    if system == "Darwin":
        from macos import Desktop
    else:
        from windows import Desktop

    # Corvid checks Unix socket directory ownership and permissions.
    parent = Path.home() if system == "Darwin" else None
    for case in cases:
        print(f"RUN {case}", flush=True)
        if case == "dmg-windows":
            from test_macos import check
            check(assets)
            continue
        with ExitStack() as stack:
            root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="cvui-", dir=parent)))
            paths = binaries(stack, system, build, assets, root)
            if case == "deck-tray":
                env = dict(os.environ, CORVID_UI_DECK_BINARY=str(paths[2]))
                subprocess.run(["cargo", "test", "--locked", "--release", "-p", "corvid-agent-deck-ui",
                                "--test", "windows_tray", "--", "--ignored", "--nocapture"],
                               cwd=ROOT, env=env, check=True)
                continue
            ui = Desktop(root) if system == "Darwin" else Desktop()
            run_case(case, root, artifacts / case, paths, ui)


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
    if args.case == "deck-focus" and not args.build:
        parser.error("deck-focus requires --build")
    if args.case == "dmg-windows" and not args.assets:
        parser.error("dmg-windows requires --assets")
    if system == "Linux" and not shutil.which("xvfb-run"):
        parser.error("install Xvfb and the Linux UI dependencies listed in docs/08_ci_validation.md")
    build = args.build.resolve() if args.build else None
    assets = args.assets.resolve() if args.assets else None
    cases = (args.case,) if args.case else SMOKE if args.suite == "smoke" else CASES[system]
    # The full suite expands with the checks supported by the selected input.
    if not args.case:
        cases = tuple(case for case in cases
                      if (case != "deck-focus" or build) and (case != "dmg-windows" or assets))
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
