"""Win32 window discovery, foreground keyboard input, and clipboard."""
from ctypes import POINTER, Structure, Union, WinDLL, WINFUNCTYPE, WinError, byref, sizeof
from ctypes import c_size_t
from ctypes import wintypes as W
from pathlib import Path
import subprocess
import time

user = WinDLL("user32", use_last_error=True)
window_callback = WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)


class KeyboardInput(Structure):
    _fields_ = [("key", W.WORD), ("scan", W.WORD), ("flags", W.DWORD),
                ("time", W.DWORD), ("extra", c_size_t)]


class MouseInput(Structure):
    _fields_ = [("x", W.LONG), ("y", W.LONG), ("data", W.DWORD),
                ("flags", W.DWORD), ("time", W.DWORD), ("extra", c_size_t)]


class InputEvent(Union):
    _fields_ = [("keyboard", KeyboardInput), ("mouse", MouseInput)]


class Input(Structure):
    _fields_ = [("kind", W.DWORD), ("event", InputEvent)]


user.EnumWindows.argtypes = [window_callback, W.LPARAM]
user.GetWindowThreadProcessId.argtypes = [W.HWND, W.LPDWORD]
user.IsWindowVisible.argtypes = [W.HWND]
user.SetForegroundWindow.argtypes = [W.HWND]
user.GetForegroundWindow.restype = W.HWND
user.SendInput.argtypes = [W.UINT, POINTER(Input), W.INT]
user.SendInput.restype = W.UINT

CONTROL = 0x11
SHIFT = 0x10
KEYS = {
    "leader": (CONTROL, 0xDD),
    "t": (ord("T"),),
    "n": (ord("N"),),
    "previous": (0xDB,),
    "paste": (CONTROL, SHIFT, ord("V")),
    "return": (0x0D,),
    "clear": (0x1B,),
}


def send(chord):
    events = [Input(1, InputEvent(keyboard=KeyboardInput(key))) for key in chord]
    events.extend(Input(1, InputEvent(keyboard=KeyboardInput(key, 0, 2)))
                  for key in reversed(chord))
    if user.SendInput(len(events), (Input * len(events))(*events), sizeof(Input)) != len(events):
        raise WinError()


class Desktop:
    def window(self, pid):
        windows = []

        @window_callback
        def find(handle, _):
            owner = W.DWORD()
            user.GetWindowThreadProcessId(handle, byref(owner))
            if owner.value == pid and user.IsWindowVisible(handle):
                windows.append(handle)
            return True

        if not user.EnumWindows(find, 0):
            raise WinError()
        return next(iter(windows), None)

    def keys(self, window, *names):
        if user.GetForegroundWindow() != window:
            user.SetForegroundWindow(window)
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline and user.GetForegroundWindow() != window:
                time.sleep(0.05)
        if user.GetForegroundWindow() != window:
            raise RuntimeError("Fixture window could not take focus on the Windows interactive desktop")
        for name in names:
            if user.GetForegroundWindow() != window:
                raise RuntimeError("Fixture window lost keyboard focus")
            send(KEYS[name])
            time.sleep(0.1)

    def submit(self, window, text, *, clear=False):
        # Read bytes from stdin so PowerShell receives the same Unicode on every code page.
        script = (
            "$ErrorActionPreference='Stop'; "
            "[Console]::InputEncoding=[Text.UTF8Encoding]::new($false); "
            "Set-Clipboard -Value ([Console]::In.ReadToEnd())"
        )
        subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
                       input=text.encode("utf-8"), creationflags=subprocess.CREATE_NO_WINDOW,
                       timeout=10, check=True)
        keys = ("clear", "paste", "return") if clear else ("paste", "return")
        self.keys(window, *keys)

    def screenshot(self, window, destination):
        subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy",
                        "Bypass", "-File", Path(__file__).with_name("capture.ps1"),
                        str(window), destination], timeout=15, check=True)
