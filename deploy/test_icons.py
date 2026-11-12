#!/usr/bin/env python3
"""Check shared artwork, installed menu ownership, and built Windows icon resources."""
import argparse
import os
from pathlib import Path
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent
SIZES = [16, 20, 24, 32, 48, 64, 128, 256]
BINARIES = [
    "corvid", "corvid-desktop", "corvid-deck", "corvid-agent-deck",
    "corvid-launch", "corvid-desktop-launch", "corvid-agent-deck-launch",
]


def artwork():
    data = (ROOT / "assets/icons/corvid.ico").read_bytes()
    assert struct.unpack_from("<HHH", data) == (0, 1, len(SIZES))
    images = []
    for index, size in enumerate(SIZES):
        width, height, _, _, planes, bits, length, offset = struct.unpack_from(
            "<BBBBHHII", data, 6 + index * 16,
        )
        assert (width or 256, height or 256, planes, bits) == (size, size, 1, 32)
        png = data[offset:offset + length]
        assert png.startswith(b"\x89PNG\r\n\x1a\n")
        assert struct.unpack_from(">II", png, 16) == (size, size)
        images.append(png)
    assert images[-1] == (ROOT / "assets/icons/corvid.png").read_bytes()
    return images


def windows_resources(build, images, runtime=None):
    import ctypes
    from ctypes import wintypes

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    user = ctypes.WinDLL("user32", use_last_error=True)
    kernel.LoadLibraryExW.argtypes = [wintypes.LPCWSTR, wintypes.HANDLE, wintypes.DWORD]
    kernel.LoadLibraryExW.restype = wintypes.HMODULE
    kernel.FindResourceW.argtypes = [wintypes.HMODULE, ctypes.c_void_p, ctypes.c_void_p]
    kernel.FindResourceW.restype = wintypes.HANDLE
    kernel.LoadResource.argtypes = [wintypes.HMODULE, wintypes.HANDLE]
    kernel.LoadResource.restype = wintypes.HANDLE
    kernel.LockResource.argtypes = [wintypes.HANDLE]
    kernel.LockResource.restype = ctypes.c_void_p
    kernel.SizeofResource.argtypes = [wintypes.HMODULE, wintypes.HANDLE]
    kernel.SizeofResource.restype = wintypes.DWORD
    kernel.FreeLibrary.argtypes = [wintypes.HMODULE]
    user.LoadImageW.argtypes = [wintypes.HINSTANCE, ctypes.c_void_p, wintypes.UINT,
                                ctypes.c_int, ctypes.c_int, wintypes.UINT]
    user.LoadImageW.restype = wintypes.HANDLE
    user.DestroyIcon.argtypes = [wintypes.HANDLE]

    def resource(module, identifier, kind):
        found = kernel.FindResourceW(module, identifier, kind)
        assert found, f"missing resource {identifier}/{kind}"
        loaded = kernel.LoadResource(module, found)
        pointer = kernel.LockResource(loaded)
        assert pointer
        return ctypes.string_at(pointer, kernel.SizeofResource(module, found))

    for name in BINARIES:
        binary = runtime if name == "corvid" and runtime else build / f"{name}.exe"
        module = kernel.LoadLibraryExW(str(binary.resolve()), None, 2)  # data only
        assert module, f"could not load {binary}: {ctypes.get_last_error()}"
        try:
            group = resource(module, 1, 14)  # GPUI's application icon ID
            assert struct.unpack_from("<HHH", group) == (0, 1, len(SIZES)), name
            for index, png in enumerate(images):
                length, identifier = struct.unpack_from("<IH", group, 6 + index * 14 + 8)
                assert length == len(png), name
                assert resource(module, identifier, 3) == png, name
            for size in [16, 32, 48, 256]:
                icon = user.LoadImageW(module, 1, 1, size, size, 0)
                assert icon, f"Windows could not decode {name}'s {size}px icon"
                user.DestroyIcon(icon)
        finally:
            kernel.FreeLibrary(module)
        print(f"PASS Windows shell/GPUI icon: {name}")


def linux_menu():
    with tempfile.TemporaryDirectory(prefix="corvid-icons-") as temporary:
        base = Path(temporary)
        root = base / 'install with spaces $cash %percent `tick` "quote" \\slash'
        data = base / "desktop data"
        first = root / "versions/first"
        second = root / "versions/second"
        for version in [first, second]:
            (version / "icons").mkdir(parents=True)
            for extension in ["svg", "png"]:
                (version / f"icons/corvid.{extension}").write_bytes(
                    (ROOT / f"assets/icons/corvid.{extension}").read_bytes(),
                )
            plugin = version / "plugins/agent-deck/corvid-agent-deck"
            plugin.parent.mkdir(parents=True)
            plugin.touch()
        current = root / "current"
        current.symlink_to(first, target_is_directory=True)
        environment = dict(os.environ, XDG_DATA_HOME=str(data))

        def install():
            subprocess.run(["sh", ROOT / "deploy/linux/install-desktop.sh", root],
                           env=environment, check=True, timeout=15)

        install()
        entry = data / "applications/Corvid.Desktop.desktop"
        before = entry.read_bytes()
        assert b"StartupWMClass=Corvid.Desktop\n" in before
        assert b"%%percent" in before and b"\\\\$cash" in before
        assert b'\\\\"quote\\\\"' in before and b"\\\\\\\\slash" in before
        assert (data / "applications/corvid-agent-deck.desktop").is_file()
        icon = data / "icons/hicolor/scalable/apps/corvid.svg"
        assert icon.resolve() == first / "icons/corvid.svg"
        current.unlink()
        current.symlink_to(second, target_is_directory=True)
        install()
        assert entry.read_bytes() == before
        assert icon.resolve() == second / "icons/corvid.svg"
        (second / "plugins/agent-deck/corvid-agent-deck").unlink()
        install()
        agent_entry = data / "applications/corvid-agent-deck.desktop"
        assert not agent_entry.exists(), "Removed plugins must not leave a dead menu entry"
        agent_entry.write_text("[Desktop Entry]\nName=Foreign Agent Deck\n")
        entry.write_text("[Desktop Entry]\nName=Another installation\n")
        icon.unlink()
        icon.write_text("foreign icon")
        install()
        assert entry.read_text() == "[Desktop Entry]\nName=Another installation\n"
        assert icon.read_text() == "foreign icon"
        assert agent_entry.read_text() == "[Desktop Entry]\nName=Foreign Agent Deck\n"
        print("PASS Linux menu escaping, update paths, and foreign-file preservation")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path)
    parser.add_argument("--runtime", type=Path)
    args = parser.parse_args()
    images = artwork()
    print("PASS shared PNG and eight ICO sizes")
    if os.name != "nt":
        linux_menu()
    if args.build:
        windows_resources(args.build, images, args.runtime)
