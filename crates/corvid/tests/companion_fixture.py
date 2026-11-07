"""Start a companion against an isolated configured host."""

import subprocess
import time

from interactive import stop


def fields(line):
    return dict(field.split("=", 1) for field in line.split() if "=" in field)


class Companion:
    def __init__(self, daemon, directory, alias=None, env=None):
        host = fields(subprocess.check_output([*daemon, "control", "host"], text=True, timeout=3))
        self.host = host["host_id"]
        self.command = [
            *daemon, "companion", "--socket-dir", directory / "presentation",
            "--state-dir", directory / "companion",
        ]
        self.env = env
        args = ["init", "--host-id", self.host]
        if alias is not None:
            args += ["--ssh-alias", alias]
        self.controller = fields(self.run(*args).stdout)["controller_id"]
        self.process = None

    def run(self, *args, check=True, **kwargs):
        return subprocess.run(
            [*self.command, *args], text=True, capture_output=True, check=check, timeout=18,
            **kwargs,
        )

    def start(self):
        self.process = subprocess.Popen([*self.command, "run"], env=self.env)
        return self.wait(lambda view: view["header"]["connection"] == "Ready")

    def slots(self):
        reply = self.run("slots", check=False)
        if reply.returncode != 0:
            return None
        header, *lines = reply.stdout.splitlines()
        return {
            "header": fields(header),
            "slots": [fields(line) for line in lines if line.startswith("slot=")],
            "bindings": [fields(line) for line in lines if line.startswith("binding ")],
            "ended": [fields(line)["run"] for line in lines if line.startswith("ended ")],
        }

    def wait(self, predicate):
        deadline = time.monotonic() + 8
        latest = None
        while time.monotonic() < deadline:
            assert self.process.poll() is None, "companion exited"
            latest = self.slots()
            if latest is not None and predicate(latest):
                return latest
            time.sleep(0.02)
        raise AssertionError(latest)

    def close(self):
        if self.process is not None:
            stop(self.process)
            self.process = None
