#!/usr/bin/env python3
"""Run isolated release-mode benchmarks against both daemon VT engines."""

import argparse
import json
import os
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[2]
TEST = "terminal::screen::benchmark::compare_engines"


def build():
    command = [
        "cargo", "test", "--locked", "--release", "--lib", "-p", "corvid-daemon",
        "--features", "ghostty-vt", "--no-run", "--message-format=json",
    ]
    result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True)
    artifacts = (json.loads(line) for line in result.stdout.splitlines())
    executable, = (
        artifact["executable"] for artifact in artifacts
        if artifact.get("reason") == "compiler-artifact"
        and artifact.get("executable") and artifact["profile"]["test"]
    )
    return executable


def measure(executable, engine, profile, sessions):
    env = os.environ | {
        "CORVID_BENCH_ENGINE": engine,
        "CORVID_BENCH_PROFILE": profile,
        "CORVID_BENCH_SESSIONS": str(sessions),
    }
    before = os.times()
    result = subprocess.run(
        [executable, "--exact", TEST, "--ignored", "--nocapture"],
        env=env, stdout=subprocess.PIPE, text=True, check=True,
    )
    after = os.times()
    payload, = (line.removeprefix("CORVID_BENCH ") for line in result.stdout.splitlines()
                if line.startswith("CORVID_BENCH "))
    report = json.loads(payload)
    if os.name == "posix":
        report["cpu_ms"] = 1000 * (
            after.children_user - before.children_user + after.children_system - before.children_system
        )
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sessions", type=int, nargs="+", default=[1, 32])
    parser.add_argument("--binary", type=Path, help="reuse a previously built daemon test binary")
    args = parser.parse_args()
    if any(not 1 <= count <= 128 for count in args.sessions):
        parser.error("session counts must be between 1 and 128")
    executable = args.binary.resolve(strict=True) if args.binary else build()
    for count in args.sessions:
        for profile in ("codex", "claude"):
            for engine in ("vt100", "ghostty"):
                print(json.dumps(measure(executable, engine, profile, count)), flush=True)


if __name__ == "__main__":
    main()
