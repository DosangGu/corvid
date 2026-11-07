"""Run the spike through real Linux PTYs with controlled byte boundaries."""

import curses
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty


INPUT = (
    b"\x1b",
    b"\x1bx",
    b"\x1b",
    b"[",
    b"A",
    *[bytes([byte]) for byte in "한글".encode()],
    b"\x1b[200~",
    "paste 한글\n".encode(),
    b"\x1b[201~",
)
QUERY = b"\x1b[5n\x1b[6n\x1b[c\x1b[18t"
REPLY = b"\x1b[0n\x1b[3;5R\x1b[?6c\x1b[8;24;80t"


def read_exact(fd, length):
    deadline = time.monotonic() + 5
    output = bytearray()
    while len(output) < length:
        timeout = max(0, deadline - time.monotonic())
        assert select.select([fd], [], [], timeout)[0], output
        chunk = os.read(fd, length - len(output))
        assert chunk, output
        output.extend(chunk)
    return bytes(output)


def child():
    assert os.environ["TERM"] == "corvid"
    curses.setupterm()
    assert curses.tigetnum("colors") == 256
    assert curses.tparm(curses.tigetstr("cup"), 2, 4) == b"\x1b[3;5H"

    cooked = termios.tcgetattr(0)
    tty.setraw(0)
    os.write(1, b"\x1b[3;5H" + QUERY)
    assert read_exact(0, len(REPLY)) == REPLY

    os.write(1, b"\x1b[H\x1b[2J\x1b[?2004h")
    for byte in "한글 e\u0301".encode():
        os.write(1, bytes([byte]))
        time.sleep(0.005)
    os.write(1, b"\r\nCORVID-READY")

    expected_input = b"".join(INPUT)
    assert read_exact(0, len(expected_input)) == expected_input
    os.write(1, b"\r\nINPUT-OK")

    termios.tcsetattr(0, termios.TCSANOW, cooked)
    tty.setcbreak(0)

    def interrupted(_signal, _frame):
        os.write(1, b"\r\nINTERRUPTED")
        sys.exit(7)

    signal.signal(signal.SIGINT, interrupted)
    wait_for_resize()
    os.write(1, b"\r\nRESIZE-OK")
    while True:
        signal.pause()


def wait_for_resize():
    deadline = time.monotonic() + 5
    while os.get_terminal_size(0) != (100, 30):
        assert time.monotonic() < deadline, "child PTY did not resize"
        time.sleep(0.02)


def read_until(fd, marker):
    deadline = time.monotonic() + 5
    output = bytearray()
    while marker not in output:
        timeout = max(0, deadline - time.monotonic())
        assert select.select([fd], [], [], timeout)[0], output
        chunk = os.read(fd, 8192)
        assert chunk, output
        output.extend(chunk)
        assert len(output) < 1024 * 1024, "unexpected output volume"
    return bytes(output)


def resize(fd, cols, rows):
    size = struct.pack("HHHH", rows, cols, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, size)


def verify_session(process, master, slave, cooked):
    initial = read_until(master, b"\x1b[?2004h")
    if b"CORVID-READY" not in initial:
        initial += read_until(master, b"CORVID-READY")
    assert "한글 e\u0301".encode() in initial, initial
    assert QUERY not in initial, initial

    for index, chunk in enumerate(INPUT):
        os.write(master, chunk)
        time.sleep(0.15 if index == 0 else 0.01)
    read_until(master, b"INPUT-OK")

    resize(slave, 100, 30)
    read_until(master, b"RESIZE-OK")
    os.write(master, b"\x03")

    final = read_until(master, b"\x1b[?1049l")
    assert b"INTERRUPTED" in final, final
    assert process.wait(timeout=5) == 7
    assert termios.tcgetattr(slave) == cooked, "outer TTY attributes changed"


def run(binary):
    terminfo = Path(__file__).resolve().parents[1] / "corvid.terminfo"
    with tempfile.TemporaryDirectory(prefix="corvid-terminfo-") as database:
        subprocess.run(["tic", "-x", "-o", database, terminfo], check=True)
        env = os.environ | {"TERM": "xterm-256color", "TERMINFO": database}
        master, slave = pty.openpty()
        try:
            cooked = termios.tcgetattr(slave)
            resize(slave, 80, 24)
            command = [binary, sys.executable, __file__, "--child"]
            with subprocess.Popen(
                command,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                env=env,
            ) as process:
                try:
                    verify_session(process, master, slave, cooked)
                finally:
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=2)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=2)
        finally:
            os.close(master)
            os.close(slave)

    print("PTY session passed input, query, resize, Ctrl-C, exit, and TTY restoration checks.")


if __name__ == "__main__":
    if sys.argv[1] == "--child":
        child()
    else:
        run(sys.argv[1])
