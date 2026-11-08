"""Measure key delivery and redraw through real local and SSH attachments."""

from dataclasses import dataclass
import math
import os
from pathlib import Path
import shlex
from statistics import median
import sys
import time
import tty

from interactive import OuterTerminal, read_exact


NS_PER_MS = 1_000_000
SAMPLES = 32
ALT_GAP_SECONDS = 0.020
ESCAPE_DELAYS = {"default": 100, "short": 40}
KEYS = {"text": b"x", "alt": b"\x1bx", "escape": b"\x1b", "late-suffix": b"x"}
CASES = {
    "default": ("text", "alt", "escape") * SAMPLES,
    "short": ("escape", "late-suffix") * SAMPLES,
}


def marker(index):
    return b"AAAAAAAA" if index % 2 == 0 else b"ZZZZZZZZ"


def echo_probe(suite, receipt_path):
    tty.setraw(0)
    os.write(1, b"INPUT-PROBE-READY")
    received = []
    for index, kind in enumerate(CASES[suite]):
        expected = KEYS[kind]
        assert read_exact(1) == expected[:1]
        first = time.monotonic_ns()
        if len(expected) > 1:
            assert read_exact(len(expected) - 1) == expected[1:]
        received.append((first, time.monotonic_ns()))
        # Every marker cell differs from the previous frame.
        os.write(1, b"\x1b[2;1H" + marker(index))
    assert read_exact(1) == b"q"
    receipt_path.write_text("".join(f"{first} {complete}\n" for first, complete in received))


@dataclass(frozen=True)
class Sample:
    kind: str
    first_write_ns: int
    suffix_write_ns: int | None
    rendered_ns: int

    @property
    def final_write_ns(self):
        return self.first_write_ns if self.suffix_write_ns is None else self.suffix_write_ns


def measure_session(outer, suite):
    outer.expect(b"INPUT-PROBE-READY")
    samples = []
    for index, kind in enumerate(CASES[suite]):
        outer.output.clear()
        key = KEYS[kind]
        suffix_write_ns = None
        first_write_ns = time.monotonic_ns()
        outer.send(key[:1])
        if kind == "alt":
            time.sleep(ALT_GAP_SECONDS)
            suffix_write_ns = time.monotonic_ns()
            outer.send(key[1:])
        outer.expect(marker(index))
        samples.append(Sample(kind, first_write_ns, suffix_write_ns, time.monotonic_ns()))
    outer.send(b"q")
    outer.wait(0)
    return samples


def report_samples(transport, suite, samples, receipt_path):
    receipts = [tuple(map(int, line.split())) for line in receipt_path.read_text().splitlines()]
    delay_ms = ESCAPE_DELAYS[suite]
    groups = {}
    for sample, (first, complete) in zip(samples, receipts, strict=True):
        assert sample.final_write_ns <= first <= complete <= sample.rendered_ns, (sample, first, complete)
        if sample.kind == "escape":
            assert first - sample.first_write_ns >= (delay_ms - 2) * NS_PER_MS, "Escape expired early"
        if sample.suffix_write_ns is not None:
            gap = sample.suffix_write_ns - sample.first_write_ns
            assert gap < delay_ms * NS_PER_MS, "probe stalled before suffix"
        groups.setdefault(sample.kind, []).append((
            (complete - sample.final_write_ns) / NS_PER_MS,
            (sample.rendered_ns - sample.final_write_ns) / NS_PER_MS,
        ))

    for kind, values in groups.items():
        pty, redraw = zip(*values)
        redraw_p95 = sorted(redraw)[math.ceil(0.95 * len(redraw)) - 1]
        print(
            f"input_probe transport={transport} escape_delay_ms={delay_ms} kind={kind} "
            f"samples={len(values)} pty_p50_ms={median(pty):.2f} "
            f"redraw_p50_ms={median(redraw):.2f} redraw_p95_ms={redraw_p95:.2f} "
            f"redraw_max_ms={max(redraw):.2f}",
            flush=True,
        )


def measure_input(ssh, command, directory):
    child = Path(__file__).resolve()
    for transport in ("local", "ssh"):
        for suite in CASES:
            label = f"input-{transport}-{suite}"
            receipts = directory / f"{label}.receipts"
            attach = [*command, "attach", "--new-workspace", label]
            if suite != "default":
                attach += ["--escape-delay-ms", str(ESCAPE_DELAYS[suite])]
            attach += ["--", sys.executable, "-B", str(child), suite, str(receipts)]
            launch = attach if transport == "local" else [*ssh, "-tt", shlex.join(attach)]
            with OuterTerminal(launch) as outer:
                samples = measure_session(outer, suite)
            report_samples(transport, suite, samples, receipts)


if __name__ == "__main__":
    suite, receipts = sys.argv[1:]
    echo_probe(suite, Path(receipts))
