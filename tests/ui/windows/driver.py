"""Win32 window discovery, foreground keyboard input, and clipboard."""
from ctypes import POINTER, Structure, Union, WinDLL, WINFUNCTYPE, WinError, byref, create_unicode_buffer, memmove, sizeof
from ctypes import c_size_t
from ctypes import wintypes as W
import json
from pathlib import Path
import subprocess
import time
from uuid import UUID

import monitor_fixture

user = WinDLL("user32", use_last_error=True)
kernel = WinDLL("kernel32", use_last_error=True)
shell = WinDLL("shell32")
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
user.AttachThreadInput.argtypes = [W.DWORD, W.DWORD, W.BOOL]
user.OpenInputDesktop.argtypes = [W.DWORD, W.BOOL, W.DWORD]
user.OpenInputDesktop.restype = W.HANDLE
user.CloseDesktop.argtypes = [W.HANDLE]
kernel.GetCurrentThreadId.restype = W.DWORD
user.SendInput.argtypes = [W.UINT, POINTER(Input), W.INT]
user.SendInput.restype = W.UINT
user.CreateWindowExW.argtypes = [W.DWORD, W.LPCWSTR, W.LPCWSTR, W.DWORD, W.INT, W.INT,
                                 W.INT, W.INT, W.HWND, W.HMENU, W.HINSTANCE, W.LPVOID]
user.CreateWindowExW.restype = W.HWND
user.DestroyWindow.argtypes = [W.HWND]
user.OpenClipboard.argtypes = [W.HWND]
user.SetClipboardData.argtypes = [W.UINT, W.HANDLE]
user.SetClipboardData.restype = W.HANDLE
kernel.GlobalAlloc.argtypes = [W.UINT, c_size_t]
kernel.GlobalAlloc.restype = W.HANDLE
kernel.GlobalLock.argtypes = [W.HANDLE]
kernel.GlobalLock.restype = W.LPVOID
kernel.GlobalUnlock.argtypes = [W.HANDLE]
kernel.GlobalFree.argtypes = [W.HANDLE]
kernel.GlobalFree.restype = W.HANDLE
shell.SHGetFolderPathW.argtypes = [W.HWND, W.INT, W.HANDLE, W.DWORD, W.LPWSTR]
shell.SHGetFolderPathW.restype = W.LONG

CONTROL = 0x11
SHIFT = 0x10
KEYS = {
    "leader": (CONTROL, 0xDD),
    "t": (ord("T"),),
    "n": (ord("N"),),
    "h": (ord("H"),),
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


def focus(window):
    if user.GetForegroundWindow() == window:
        return
    desktop = user.OpenInputDesktop(0, False, 1)  # DESKTOP_READOBJECTS
    if not desktop:
        raise RuntimeError(f"Fixture cannot access the Windows input desktop: {WinError()}")
    user.CloseDesktop(desktop)
    user.SetForegroundWindow(window)
    foreground = user.GetForegroundWindow()
    if foreground and foreground != window:
        current = kernel.GetCurrentThreadId()
        active = user.GetWindowThreadProcessId(foreground, None)
        if active and active != current and user.AttachThreadInput(current, active, True):
            try:
                user.SetForegroundWindow(window)
            finally:
                user.AttachThreadInput(current, active, False)
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline and user.GetForegroundWindow() != window:
        time.sleep(0.05)
    if user.GetForegroundWindow() != window:
        raise RuntimeError(f"Cannot activate fixture window {window}; foreground window is {foreground}")


def clipboard(text):
    # The clipboard API needs a window owned by the process taking ownership.
    hwnd_message = W.HWND(-3)
    owner = user.CreateWindowExW(0, "STATIC", None, 0, 0, 0, 0, 0,
                                 hwnd_message, None, None, None)
    if not owner:
        raise WinError()
    try:
        deadline = time.monotonic() + 3
        while not user.OpenClipboard(owner):
            if time.monotonic() >= deadline:
                raise WinError()
            time.sleep(0.05)
        try:
            clipboard_text(text)
        finally:
            user.CloseClipboard()
    finally:
        user.DestroyWindow(owner)


def clipboard_text(text):
    gmem_moveable = 2
    cf_unicode_text = 13
    data = text.encode("utf-16-le") + b"\0\0"
    handle = kernel.GlobalAlloc(gmem_moveable, len(data))
    if not handle:
        raise WinError()
    try:
        address = kernel.GlobalLock(handle)
        if not address:
            raise WinError()
        try:
            memmove(address, data, len(data))
        finally:
            kernel.GlobalUnlock(handle)
        if not user.EmptyClipboard() or not user.SetClipboardData(cf_unicode_text, handle):
            raise WinError()
        handle = None  # The window manager now owns the clipboard allocation.
    finally:
        if handle:
            kernel.GlobalFree(handle)


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
        focus(window)
        for name in names:
            if user.GetForegroundWindow() != window:
                raise RuntimeError("Fixture window lost keyboard focus")
            send(KEYS[name])
            time.sleep(0.1)

    def submit(self, window, text, *, clear=False):
        clipboard(text)
        keys = ("clear", "paste", "return") if clear else ("paste", "return")
        self.keys(window, *keys)

    def fixture_shortcuts(self, state):
        csidl_programs = 2
        buffer = create_unicode_buffer(260)
        if shell.SHGetFolderPathW(None, csidl_programs, None, 0, buffer) != 0:
            raise RuntimeError("Could not locate the Windows Start Menu Programs folder")
        programs = Path(buffer.value)
        for record in monitor_fixture.records(state):
            controller = UUID(json.loads(record.read_text(encoding="utf-8"))["controller"])
            path = programs / f"Corvid Agent Deck {controller.hex[:8]}.lnk"
            if path.exists():
                raise FileExistsError(f"Fixture shortcut already exists: {path}")
            yield path

    def screenshot(self, window, destination):
        subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy",
                        "Bypass", "-File", Path(__file__).with_name("capture.ps1"),
                        str(window), destination], timeout=15, check=True)
