#!/usr/bin/env python3
"""Measures monitrs' own idle CPU, resident memory and open files from outside it.

§16.1 budgets "idle self CPU: median below 1%, 95th percentile below 2%",
"resident memory: below 50 MiB in the default configuration", and no unbounded
file-descriptor growth. Measuring those with monitrs' own collector would be
measuring the thing with itself, so this uses `ps` and `lsof`: independent
observers, and `ps`'s %CPU is the number a user would look at.

Runs the release binary on a pty at 160x48 with `--no-config` (the default
configuration §16.1 names), leaves it completely idle, and samples once a second.
The first ten seconds are discarded, because startup work is not idle.

    cargo build --release -p monitrs
    python3 scripts/measure-overhead.py

The pty is drained throughout. Without that the program blocks on a full buffer and
the script measures a stalled process rather than an idle one.

Set `MONITRS_SCREEN_KEY` to a single keystroke to switch away from the Overview
screen before measuring, and leave it in place for the whole run — for example, to
measure with the Battery screen visible (§8.6's sensor group runs on a 5-second
cadence rather than 30 while that screen is up):

    MONITRS_SCREEN_KEY=7 python3 scripts/measure-overhead.py

The key is written into the pty a second after start-up, comfortably inside the
discarded warm-up window, so the requested screen is the one idling for the entire
measured duration. Unset, the script measures the Overview screen, untouched, which
is its default and the row §16.1's idle budget is about.

Exits non-zero if any budget is missed, and prints which. Note that the result
depends heavily on the machine: the cost is dominated by per-process and per-device
OS reads, so a host with a thousand processes costs several times what §16.1's
200-process reference workload does. See docs/benchmarks.md.
"""

import os
import pty
import re
import select
import signal
import statistics
import subprocess
import sys
import time

# Relative to the repository root, which is where this is meant to be run from.
BINARY = os.environ.get("MONITRS_BINARY", "target/release/monitrs")
COLUMNS, LINES = 160, 48
WARMUP_SECONDS = 10
MEASURE_SECONDS = 60
# Unset: measure the Overview screen, untouched. Set (e.g. "7" for Battery): send
# that keystroke once, shortly after start-up, so the requested screen is the one
# idling for the whole measurement.
SCREEN_KEY = os.environ.get("MONITRS_SCREEN_KEY")


def ps_sample(pid):
    """(%cpu, rss_bytes) for pid, or None once it is gone."""
    out = subprocess.run(
        ["ps", "-o", "%cpu=,rss=", "-p", str(pid)],
        capture_output=True,
        text=True,
    )
    fields = out.stdout.split()
    if len(fields) != 2:
        return None
    return float(fields[0]), int(fields[1]) * 1024


def percentile(values, fraction):
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(len(ordered) * fraction))
    return ordered[index]


def main():
    primary, secondary = pty.openpty()
    # A real terminal size, so the app takes the Wide layout rather than the
    # too-small notice.
    import fcntl
    import struct
    import termios

    fcntl.ioctl(secondary, termios.TIOCSWINSZ, struct.pack("HHHH", LINES, COLUMNS, 0, 0))

    environment = dict(os.environ, TERM="xterm-256color", COLUMNS=str(COLUMNS), LINES=str(LINES))
    # No config file, so this is the default configuration §16.1 names.
    child = subprocess.Popen(
        [BINARY, "--no-config"],
        stdin=secondary,
        stdout=secondary,
        stderr=secondary,
        env=environment,
        close_fds=True,
    )
    os.close(secondary)

    cpu, rss, fds = [], [], []
    drained = bytearray()
    try:
        deadline = time.monotonic() + WARMUP_SECONDS + MEASURE_SECONDS
        measure_from = time.monotonic() + WARMUP_SECONDS
        screen_key_sent = SCREEN_KEY is None
        screen_key_at = time.monotonic() + 1
        while time.monotonic() < deadline:
            # Sent once, a second in: late enough that the app is reading its
            # input, early enough that it lands well before `measure_from` — so
            # the requested screen, not the Overview default, is what idles for
            # the entire measured window.
            if not screen_key_sent and time.monotonic() >= screen_key_at:
                os.write(primary, SCREEN_KEY.encode())
                screen_key_sent = True
            # The pty has to be read or the app blocks on a full buffer, which would
            # measure a stalled process rather than an idle one.
            ready, _, _ = select.select([primary], [], [], 0.25)
            if ready:
                try:
                    drained += os.read(primary, 1 << 16)
                except OSError:
                    break
            if child.poll() is not None:
                print(f"monitrs exited early with {child.returncode}", file=sys.stderr)
                break
            if time.monotonic() >= measure_from:
                sample = ps_sample(child.pid)
                if sample is None:
                    break
                cpu.append(sample[0])
                rss.append(sample[1])
                open_files = subprocess.run(
                    ["lsof", "-p", str(child.pid)], capture_output=True, text=True
                )
                fds.append(max(0, len(open_files.stdout.splitlines()) - 1))
                time.sleep(0.75)
    finally:
        if child.poll() is None:
            # `q` is the documented quit key, and quitting through it is what lets this
            # script check that the alternate screen was left behind. The pty has to go
            # on being drained while the program shuts down: it writes its final frames
            # and its restore sequence on the way out, and a full buffer would stall it
            # into the SIGTERM path — which is how an earlier version of this script
            # reported that the screen had never been restored.
            os.write(primary, b"q")
            deadline = time.monotonic() + 5
            while child.poll() is None and time.monotonic() < deadline:
                ready, _, _ = select.select([primary], [], [], 0.1)
                if ready:
                    try:
                        drained += os.read(primary, 1 << 16)
                    except OSError:
                        break
            if child.poll() is None:
                child.send_signal(signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except Exception:
                    child.kill()
        # One last drain, so the restore sequence is in `drained` even if it arrived
        # after the process exited.
        while True:
            ready, _, _ = select.select([primary], [], [], 0.2)
            if not ready:
                break
            try:
                chunk = os.read(primary, 1 << 16)
            except OSError:
                break
            if not chunk:
                break
            drained += chunk
        os.close(primary)

    if not cpu:
        print("no samples taken", file=sys.stderr)
        return 1

    text = drained.decode("utf-8", "replace")
    print(f"screen: {SCREEN_KEY!r} (unset = Overview, the default)")
    print(f"samples: {len(cpu)} over {MEASURE_SECONDS}s idle, after {WARMUP_SECONDS}s warm-up")
    print(f"exit code: {child.returncode}")
    print(f"alternate screen entered: {'?1049h' in text}, left: {'?1049l' in text}")
    print(f"idle self CPU: median {statistics.median(cpu):.2f}%  p95 {percentile(cpu, 0.95):.2f}%  max {max(cpu):.2f}%")
    mib = 1024 * 1024
    print(
        f"resident memory: median {statistics.median(rss) / mib:.1f} MiB  "
        f"max {max(rss) / mib:.1f} MiB"
    )
    print(f"open files: first {fds[0]}  last {fds[-1]}  max {max(fds)}")
    print(f"pty bytes drained: {len(drained)}")

    verdicts = [
        ("quit through `q`", child.returncode == 0),
        ("alternate screen left", "?1049l" in text),
        ("idle CPU median < 1%", statistics.median(cpu) < 1.0),
        ("idle CPU p95 < 2%", percentile(cpu, 0.95) < 2.0),
        ("RSS < 50 MiB", max(rss) < 50 * mib),
        ("no file-descriptor growth", fds[-1] <= fds[0] + 2),
    ]
    for label, ok in verdicts:
        print(f"  {'PASS' if ok else 'FAIL'}  {label}")
    return 0 if all(ok for _, ok in verdicts) else 1


if __name__ == "__main__":
    sys.exit(main())
