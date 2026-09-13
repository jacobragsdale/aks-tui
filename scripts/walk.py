#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte"]
# ///
"""Drives aks-tui under a pty against scripts/fake-kubectl and prints what it
painted, so a change can be seen end to end without a cluster.

    scripts/walk.py                 # the release binary, a scratch config
    scripts/walk.py --keys '2 ] / orders'

Every frame is read through pyte, so what is asserted is the screen a person
would see. Exits 1 when an expectation is not met.
"""
import argparse
import fcntl
import os
import pty
import select
import struct
import sys
import tempfile
import termios
import time

import pyte

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COLUMNS, LINES = 140, 40

CONFIG = """
[[clusters]]
name = "qa"
context = "aks-qa"
namespaces = ["dev", "qa", "uat"]

[[clusters]]
name = "prod"
context = "aks-prod"
namespaces = ["prod"]
"""


class Walk:
    def __init__(self, binary, scratch, env):
        self.screen = pyte.Screen(COLUMNS, LINES)
        self.stream = pyte.ByteStream(self.screen)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(binary, [binary], env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", LINES, COLUMNS, 0, 0))
        self.scratch = scratch

    def pump(self, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            # crossterm asks where the cursor is on startup and waits for the
            # answer; a pty with nobody on the other end would hang it.
            if b"\x1b[6n" in data:
                os.write(self.fd, b"\x1b[1;1R")
            self.stream.feed(data)

    def text(self):
        return "\n".join(self.screen.display)

    def send(self, keys):
        os.write(self.fd, keys.encode())

    def expect(self, needle, seconds=5.0):
        deadline = time.time() + seconds
        while time.time() < deadline:
            self.pump(0.2)
            if needle in self.text():
                return True
        print(f"--- expected {needle!r}, screen was:\n{self.text()}", file=sys.stderr)
        return False

    def quit(self):
        self.send("q")
        self.pump(1)
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=os.path.join(ROOT, "target", "release", "aks-tui"))
    parser.add_argument("--keys", default="", help="keys to press after the first frame, space separated")
    parser.add_argument("--show", action="store_true", help="print the final screen")
    options = parser.parse_args()

    scratch = tempfile.mkdtemp(prefix="aks-tui-walk-")
    os.makedirs(os.path.join(scratch, "aks-tui"))
    # The app runs `kubectl`; the fake answers to that name from a scratch bin.
    os.makedirs(os.path.join(scratch, "bin"))
    os.symlink(os.path.join(ROOT, "scripts", "fake-kubectl"), os.path.join(scratch, "bin", "kubectl"))
    with open(os.path.join(scratch, "aks-tui", "config.toml"), "w") as f:
        f.write(CONFIG)
    env = dict(os.environ)
    env.update({
        "PATH": os.path.join(scratch, "bin") + os.pathsep + env.get("PATH", ""),
        "XDG_CONFIG_HOME": scratch,
        "XDG_DATA_HOME": scratch,
        "FAKE_KUBECTL_LOG": scratch,
        "TERM": "xterm-256color",
        "COLUMNS": str(COLUMNS),
        "LINES": str(LINES),
    })
    env.pop("NO_COLOR", None)

    walk = Walk(options.binary, scratch, env)
    ok = True
    ok &= walk.expect("1 qa/dev")
    ok &= walk.expect("orders-worker-5c4d3e-q8zt")
    ok &= walk.expect("1 qa/dev ✗ 1")
    ok &= walk.expect("3 qa/uat ✗ 1")
    ok &= walk.expect("5 pods")
    for key in options.keys.split():
        walk.send({"Enter": "\r", "Esc": "\x1b", "Tab": "\t", "Space": " "}.get(key, key))
        walk.pump(0.4)
    if not options.keys:
        walk.send("4")
        ok &= walk.expect("orders-api-7d9f5b-prd02")
        ok &= walk.expect("Terminating")
        walk.send("]")
        ok &= walk.expect("orders-worker-5c4d3e-q8zt")
        walk.send("/worker\r")
        ok &= walk.expect("1/5 · Name")
        ok &= walk.expect("CrashLoopBackOff  ↻17")
        walk.send("\x1b")
        walk.send("3")
        ok &= walk.expect("ImagePullBackOff")
        ok &= walk.expect("2 pods")
        walk.send("?")
        ok &= walk.expect("read this tab again")
        walk.send("\x1b")
    if options.show:
        print(walk.text())
    walk.quit()

    with open(os.path.join(scratch, "calls.log")) as f:
        calls = f.read().splitlines()
    print(f"{len(calls)} kubectl calls; first: {calls[0] if calls else '-'}")
    wanted = "--context aks-qa --request-timeout=10s get pods -o json -n dev"
    if wanted not in calls:
        print(f"--- expected a call {wanted!r} in {calls}", file=sys.stderr)
        ok = False
    cache = os.path.join(scratch, "aks-tui", "cache.json")
    if not os.path.exists(cache):
        print("--- no cache was written on quit", file=sys.stderr)
        ok = False
    print("ok" if ok else "FAILED")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
