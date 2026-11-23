"""Quartz window discovery and native Cocoa input."""
from pathlib import Path
import subprocess


class Desktop:
    def __init__(self, root):
        self.helper = root / "macos-ui"
        subprocess.run(["swiftc", Path(__file__).with_suffix(".swift"), "-o", self.helper],
                       check=True, timeout=90)

    def window(self, pid):
        window = subprocess.check_output([self.helper, "window", str(pid)], timeout=5).strip()
        if window:
            return str(pid), window.decode("ascii")

    def submit(self, window, text, *, clear=False):
        keys = ("clear", "paste", "return") if clear else ("paste", "return")
        subprocess.run([self.helper, "submit", window[0], *keys], input=text.encode("utf-8"),
                       check=True, timeout=10)

    def screenshot(self, window, destination):
        subprocess.run(["screencapture", "-x", "-l", window[1], destination],
                       check=True, timeout=10)
