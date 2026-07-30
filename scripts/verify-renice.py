#!/usr/bin/env python3
"""Drives the real interface through the renice flow and checks that the kernel agreed.

    cargo build --release -p monitrs
    python3 scripts/verify-renice.py

`crates/monitrs-collectors/src/renice.rs` has live tests that renice the test process
and read the value back, so the *module* is covered. What no test covers is pressing
`R` in the assembled interface, and that is where this project's bugs have lived: the
capability said `Available`, the module worked, and the effect handler refused. This
script closes that gap the only way it can be closed — by driving the real binary on a
pty and then asking `ps`, not monitrs, whether anything changed.

It starts its own `sleep` as the target and filters to that PID (§7.2 makes the PID one
of the four matched fields), so nothing on the machine is disturbed, and it raises the
niceness rather than lowering it because raising your own process needs no privileges.

Exits non-zero if the dialog does not appear, if it does not name the full identity, or
if the value did not actually change.
"""

import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

BINARY = "target/release/monitrs"
COLUMNS, LINES = 160, 48


def nice_of(pid):
    out = subprocess.run(["ps", "-o", "nice=", "-p", str(pid)], capture_output=True, text=True)
    field = out.stdout.strip()
    return int(field) if field else None


def main():
    import atexit

    target = subprocess.Popen(["sleep", "600"])
    atexit.register(lambda: (target.kill(), target.wait()) if target.poll() is None else None)
    time.sleep(0.3)
    before = nice_of(target.pid)
    print(f"target pid {target.pid}, nice before: {before}")
    if before is None:
        target.kill()
        print("could not read the target's nice value", file=sys.stderr)
        return 1

    primary, secondary = pty.openpty()
    fcntl.ioctl(secondary, termios.TIOCSWINSZ, struct.pack("HHHH", LINES, COLUMNS, 0, 0))
    child = subprocess.Popen(
        [BINARY, "--no-config", "--ascii", "--color", "off"],
        stdin=secondary,
        stdout=secondary,
        stderr=secondary,
        env=dict(os.environ, TERM="xterm-256color"),
        close_fds=True,
    )
    os.close(secondary)

    screen = bytearray()

    def drain(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([primary], [], [], 0.1)
            if ready:
                try:
                    screen.extend(os.read(primary, 1 << 16))
                except OSError:
                    return

    def send(keys, settle=0.6):
        os.write(primary, keys)
        drain(settle)

    try:
        # Two samples, so the table has real values and a selection.
        drain(3.0)
        # Filter to the target's PID, which §7.2 makes one of the four matched fields.
        send(b"/", 0.4)
        send(str(target.pid).encode(), 0.4)
        send(b"\r", 1.2)
        after_filter = screen.decode("utf-8", "replace")
        assert str(target.pid) in after_filter, "the filtered table must show the target"

        # `R` proposes, `j` raises the value, Enter accepts the value, Enter confirms.
        send(b"R", 0.8)
        stage = screen.decode("utf-8", "replace")
        # The panel is titled CONFIRM and its second half asks for the value; it never
        # uses the word "renice", which is what an earlier version of this script
        # looked for and wrongly concluded from.
        if "CHOOSE A PRIORITY" not in stage:
            print("no renice dialog appeared", file=sys.stderr)
            print(stage[-3000:])
            return 1
        assert str(target.pid) in stage, "the dialog must name the process it is about"
        assert "start key" in stage, "§15.1: the dialog must show the full identity"
        print("renice dialog opened, naming the identity")
        send(b"jj", 0.5)
        send(b"\r", 0.6)
        send(b"\r", 1.5)
    finally:
        if child.poll() is None:
            os.write(primary, b"q")
            deadline = time.monotonic() + 5
            while child.poll() is None and time.monotonic() < deadline:
                drain(0.2)
            if child.poll() is None:
                child.send_signal(signal.SIGTERM)
        drain(0.5)
        os.close(primary)

    after = nice_of(target.pid)
    text = screen.decode("utf-8", "replace")
    target.kill()
    target.wait()

    print(f"nice after: {after}")
    # The status line should say what happened, whichever way it went.
    notices = [
        line.strip()
        for line in re.split(r"[\r\n]+", re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", text))
        if "nice" in line.lower() and line.strip()
    ]
    for line in notices[-4:]:
        print(f"  screen: {line[:150]}")

    if after is None:
        print("FAIL: the target vanished", file=sys.stderr)
        return 1
    if after == before:
        print(f"FAIL: nice unchanged at {before} — the interface did not renice", file=sys.stderr)
        return 1
    print(f"PASS: the kernel agrees — nice went {before} -> {after} through the interface")
    return 0


if __name__ == "__main__":
    sys.exit(main())
