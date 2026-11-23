"""Quartz window discovery and native Cocoa input."""
from pathlib import Path
import shutil
import subprocess

from macos_package import plist, sign


def bundle_deck(source, root):
    # UserNotifications requires a bundle even when testing a local Cargo build.
    app = root / "Corvid Agent Deck.app"
    binary = app / "Contents/MacOS/corvid-agent-deck"
    binary.parent.mkdir(parents=True)
    shutil.copy2(source, binary)
    plist(app, "Corvid Agent Deck", binary.name, "org.corvid.agent-deck.ui-test",
          "0.0.0", background=True)
    sign(app)
    return binary


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

    def keys(self, window, *keys):
        subprocess.run([self.helper, "keys", window[0], *keys], check=True, timeout=10)

    def screenshot(self, window, destination):
        subprocess.run(["screencapture", "-x", "-l", window[1], destination],
                       check=True, timeout=10)
