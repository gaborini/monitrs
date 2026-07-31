#!/usr/bin/env python3
"""Measures monitrs' own idle CPU, resident memory and open files from outside it.

§16.1 budgets "idle self CPU: median below 1%, 95th percentile below 2%",
"resident memory: below 50 MiB in the default configuration", and no unbounded
file-descriptor growth. Measuring those with monitrs' own collector would be
measuring the thing with itself, so this uses independent observers — `ps`,
`/proc`, `lsof`, depending on platform, never monitrs.

Runs the release binary on a pty at 160x48 with `--no-config` (the default
configuration §16.1 names — which is also, as it happens, §16.1's reference
sampling interval and history span: one second and five minutes, see
`crates/monitrs/src/config.rs`'s `Duration::from_secs(1)` and `from_secs(300)`),
leaves it completely idle, and samples roughly every 0.81 s on Darwin — a 0.25 s
select() poll plus a 0.75 s sleep between reads, so a 60-second window yields
about 74 samples, not 60. That is not a cosmetic detail on Darwin: `ps`'s %cpu is
reported over roughly that ~0.81 s window, not a clean second, so turning a
reported percentage back into an absolute millisecond figure (or vice versa) has
to use the real interval, not 1000 ms — see docs/benchmarks.md's "Why there are
two idle rows" for a case where using the wrong one mattered. The first ten
seconds are discarded, because startup work is not idle.

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

# Two platforms, two sources, one definition

Read this before comparing a Linux figure against a Darwin one from this script.

On Darwin, `ps -o %cpu=` is a short-time-constant *decaying average*: recent CPU
use dominates and older use fades out, which is exactly what lets a single
periodic burst show up as an elevated sample near where it happened and nowhere
else — the property that makes a p95 taken from it mean something for a bursty
workload like an idle sampling loop.

On Linux, `ps %cpu` is a **lifetime average**: `(utime + stime) / (wall-clock time
since the process started)`, per `ps(1)` and `proc(5)`. A process that has been up
for ten minutes cannot show a burst that happened in its last second — the ten
minutes before it dilute the average past visibility, and the longer the process
has run the worse this gets. A p95 computed from *that* number would not be a
noisier version of the Darwin figure; it would be answering a different question
that happens to be reported in the same unit. Publishing the two side by side as
though they were the same measurement is the one outcome this script is written to
avoid.

So the Linux path does not call `ps` on the measured process at all. It reads
`/proc/<pid>/stat`'s `utime` and `stime` fields directly — the same two fields
`ps` itself sums — and takes the delta between two consecutive readings divided by
the wall-clock time actually elapsed between them (measured with the same
monotonic clock the sampling loop already uses, not assumed to be any particular
interval): a **true CPU-time share for the sampling interval**, which is what
§16.1's "idle self CPU" means in the first place. That makes the Linux figure
*better founded* than the Darwin one, which is still an average whose decay
constant `ps(1)` does not document, rather than merely a different one.

Resident memory follows the same reasoning `crates/monitrs-collectors/src/linux/
process.rs` already recorded for the live collector: `/proc/<pid>/stat`'s own RSS
field (24) is in *pages*, and turning that into bytes needs the page size and
therefore `libc`. `/proc/<pid>/status`'s `VmRSS` line is already in kibibytes, so
that is what this script reads, exactly as `selfstat.rs` reads its own `VmRSS` the
same way.

Open descriptors are counted from `/proc/<pid>/fd` rather than `lsof`, for the
reason `crates/monitrs-collectors/src/selfstat.rs` gives for reading its own
descriptors the same way: a minimal Linux image may not carry `lsof` at all, and
`/proc/<pid>/fd` always exists for a process you own.

The one thing this script still shells out to `ps` for on Linux is the host's
*total* process count (`ps -A -o pid=`), printed beside every run so a reader can
see which workload it ran against. A bare count is not an average of anything, so
it carries none of the problem above, and `-A -o pid=` is the same portable
invocation on both platforms.

# Report the workload beside the verdict

This release already published one figure measured against 981-1007 processes —
five times §16.1's 200-process reference workload — next to the 200-process
budget, because nothing printed the workload a run was taken against. So every run
of this script now prints, beside its pass/fail lines: logical CPU count, host
process count, terminal size, monitrs' own sampling interval and history span (the
compiled defaults `--no-config` uses, not measured per run because nothing here
changes them), and whether all of that is §16.1's own reference workload — 8
logical CPUs, 200 processes, a one-second interval, a five-minute history — or
some other one. A run against some other workload is not wrong, but it must not be
read as a reading of the budget.
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
# Panel titles unique to a given screen, so a screen switch can be confirmed from
# the pty output rather than merely inferred from RSS/byte-count side effects.
# Only the key this script documents has one; an unrecognised key is not checked
# rather than guessed at.
SCREEN_MARKERS = {"7": "BATTERY"}  # crates/monitrs-tui/src/views/battery.rs's panel title

IS_LINUX = sys.platform.startswith("linux")

# §16.1's own reference workload — see the module docstring's "Report the
# workload beside the verdict". Interval and history are not something this
# script measures at runtime: `--no-config` gives monitrs' compiled defaults
# (`crates/monitrs/src/config.rs`), which already are these two figures.
REFERENCE_LOGICAL_CPUS = 8
REFERENCE_PROCESSES = 200
REFERENCE_INTERVAL_SECONDS = 1
REFERENCE_HISTORY_SECONDS = 300
MONITRS_INTERVAL_SECONDS = 1
MONITRS_HISTORY_SECONDS = 300

# `USER_HZ`: the clock-tick rate `/proc/<pid>/stat`'s `utime`/`stime` are counted
# in. 100 on every architecture Linux supports in practice — the same fact
# `crates/monitrs-collectors/src/linux/process.rs` and `enrich.rs`'s
# `DEFAULT_USER_HZ` record — and `sysconf(_SC_CLK_TCK)` is the authoritative
# source when it is available, which is why that is tried first.
_DEFAULT_USER_HZ = 100
if IS_LINUX:
    try:
        CLK_TCK = os.sysconf("SC_CLK_TCK")
    except (AttributeError, OSError, ValueError):
        CLK_TCK = _DEFAULT_USER_HZ
    if not CLK_TCK or CLK_TCK <= 0:
        CLK_TCK = _DEFAULT_USER_HZ
else:
    CLK_TCK = None


def darwin_ps_sample(pid):
    """(%cpu, rss_bytes) for pid on Darwin, or None once it is gone.

    `ps -o %cpu=` here is Darwin's short-time-constant decaying average — see the
    module docstring for why that is exactly what makes a p95 taken from it
    meaningful, and why the same flag means something else on Linux.
    """
    out = subprocess.run(
        ["ps", "-o", "%cpu=,rss=", "-p", str(pid)],
        capture_output=True,
        text=True,
    )
    fields = out.stdout.split()
    if len(fields) != 2:
        return None
    return float(fields[0]), int(fields[1]) * 1024


def linux_cpu_ticks(pid):
    """(utime, stime) in `USER_HZ` clock ticks from `/proc/<pid>/stat`, or None
    once the process is gone or the line cannot be parsed.

    The kernel writes the process name in parentheses, unescaped, and a name may
    itself contain spaces or a `)` — so the split point is the *last* `)` in the
    line rather than plain whitespace splitting, the same rule
    `crates/monitrs-collectors/src/linux/process.rs`'s `parse_pid_stat` documents
    for the same file. `proc(5)` numbers fields from 1; field 3 (state) is the
    first token after that `)`, so `utime` (field 14) and `stime` (field 15) land
    at indices 11 and 12 of the split tail.
    """
    try:
        with open(
            f"/proc/{pid}/stat", "r", encoding="utf-8", errors="replace"
        ) as handle:
            line = handle.read()
    except OSError:
        return None
    close = line.rfind(")")
    if close == -1:
        return None
    tail = line[close + 1 :].split()
    if len(tail) < 13:  # need through field 15, i.e. tail index 12
        return None
    try:
        return int(tail[11]), int(tail[12])
    except ValueError:
        return None


def linux_vm_rss_bytes(pid):
    """`VmRSS` from `/proc/<pid>/status`, in bytes, or None once the process is
    gone or the line is missing or unparsable.

    Not `/proc/<pid>/stat`'s field 24: that is RSS in *pages*, and converting it
    needs the page size and therefore `libc` — the same reason
    `crates/monitrs-collectors/src/linux/process.rs` reads RSS from `status`
    rather than `stat` for the live collector. `status` already reports
    kibibytes, exactly as `selfstat.rs` reads its own `VmRSS` the same way.
    """
    try:
        with open(
            f"/proc/{pid}/status", "r", encoding="utf-8", errors="replace"
        ) as handle:
            status = handle.read()
    except OSError:
        return None
    for line in status.splitlines():
        if not line.startswith("VmRSS:"):
            continue
        fields = line[len("VmRSS:") :].split()
        if len(fields) != 2 or fields[1] != "kB":
            return None
        try:
            return int(fields[0]) * 1024
        except ValueError:
            return None
    return None


def linux_raw_sample(pid):
    """(utime_ticks, stime_ticks, rss_bytes) for pid, or None once it is gone."""
    ticks = linux_cpu_ticks(pid)
    if ticks is None:
        return None
    rss = linux_vm_rss_bytes(pid)
    if rss is None:
        return None
    return ticks[0], ticks[1], rss


def linux_open_descriptors(pid):
    """Number of entries in `/proc/<pid>/fd`, or None once the process is gone.

    Not `lsof`: a minimal Linux image may not carry it, and `/proc/<pid>/fd`
    always exists for a process you own. `crates/monitrs-collectors/src/
    selfstat.rs` reads its own descriptors the same way.
    """
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return None


def darwin_open_descriptors(pid):
    """Number of open descriptors for pid on Darwin, via `lsof`."""
    open_files = subprocess.run(
        ["lsof", "-p", str(pid)], capture_output=True, text=True
    )
    return max(0, len(open_files.stdout.splitlines()) - 1)


def system_process_count():
    """Total live processes on this host, via `ps -A -o pid=`.

    A bare enumeration, not an average of anything, so it does not carry the
    %cpu problem the module docstring describes — it is portable to both
    platforms unchanged, and it is the number that says whether this run
    approximates §16.1's 200-process reference workload or the ~1000-process
    hard case every other figure in this release was measured against.
    """
    out = subprocess.run(
        ["ps", "-A", "-o", "pid="], capture_output=True, text=True
    )
    return len(out.stdout.split())


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
    # Timestamps of accepted samples, so this script's own poll cadence can be
    # reported measured rather than merely asserted — see "Two platforms, two
    # sources" above for why the app's 1 s sampling interval and this script's
    # ~0.81 s poll cadence must never be conflated.
    sample_times = []
    # Linux only: (cpu_seconds_so_far, monotonic_time) of the previous reading, so
    # a delta can be taken. A rate needs two readings; the first pass through the
    # sampling block below seeds this and records no sample.
    linux_previous = None
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
                if IS_LINUX:
                    current = linux_raw_sample(child.pid)
                    if current is None:
                        break
                    now = time.monotonic()
                    cpu_seconds = (current[0] + current[1]) / CLK_TCK
                    if linux_previous is not None:
                        prev_cpu_seconds, prev_time = linux_previous
                        elapsed = now - prev_time
                        # Unlike `ps`'s %cpu, this reading needs no assumption
                        # about the sampling interval at all: it divides by the
                        # interval actually elapsed, measured with the same
                        # monotonic clock the outer loop uses.
                        cpu_percent = (
                            100.0 * (cpu_seconds - prev_cpu_seconds) / elapsed
                            if elapsed > 0
                            else 0.0
                        )
                        descriptors = linux_open_descriptors(child.pid)
                        if descriptors is None:
                            break
                        cpu.append(cpu_percent)
                        rss.append(current[2])
                        fds.append(descriptors)
                        sample_times.append(now)
                    linux_previous = (cpu_seconds, now)
                else:
                    sample = darwin_ps_sample(child.pid)
                    if sample is None:
                        break
                    cpu.append(sample[0])
                    rss.append(sample[1])
                    fds.append(darwin_open_descriptors(child.pid))
                    sample_times.append(time.monotonic())
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

    # The workload, printed beside the verdict rather than left to be assumed.
    # See the module docstring's "Report the workload beside the verdict".
    logical_cpus = os.cpu_count()
    process_count = system_process_count()
    poll_cadence = (
        statistics.mean(b - a for a, b in zip(sample_times, sample_times[1:]))
        if len(sample_times) > 1
        else None
    )
    # Logical CPUs must match exactly — §16.1 names a specific instance shape,
    # not a range. Process count is inherently a moving figure even on the
    # reference host itself, so "matches" allows the same order of magnitude
    # (within 50%) rather than exact equality: generous enough that ordinary
    # variation does not read as a mismatch, tight enough that this release's
    # own 981-1007-vs-200 mistake would still be caught.
    matches_reference = (
        logical_cpus == REFERENCE_LOGICAL_CPUS
        and process_count is not None
        and abs(process_count - REFERENCE_PROCESSES) <= REFERENCE_PROCESSES * 0.5
    )
    print(
        f"workload: {logical_cpus} logical CPUs, {process_count} processes, "
        f"{COLUMNS}x{LINES} terminal"
    )
    print(
        f"monitrs: {MONITRS_INTERVAL_SECONDS}s sampling interval, "
        f"{MONITRS_HISTORY_SECONDS}s ({MONITRS_HISTORY_SECONDS // 60} min) history "
        "-- compiled defaults, unchanged by --no-config"
    )
    if poll_cadence is not None:
        print(
            f"this script's own poll cadence: measured mean {poll_cadence:.2f}s "
            f"over {len(sample_times)} samples -- not monitrs' sampling interval "
            "above, see the module docstring"
        )
    if matches_reference:
        print(
            f"workload matches §16.1's reference (8 logical CPUs, ~200 processes, "
            f"{REFERENCE_INTERVAL_SECONDS}s interval, {REFERENCE_HISTORY_SECONDS}s "
            "history): the figures below are a reading of the budget itself"
        )
    else:
        print(
            "workload does NOT match §16.1's reference (8 logical CPUs, ~200 "
            "processes) -- the figures below are some other workload's, useful as "
            "a hard case but not as a reading of the budget"
        )

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
    marker = SCREEN_MARKERS.get(SCREEN_KEY)
    if marker is not None:
        verdicts.append((f"screen key {SCREEN_KEY!r} landed ({marker!r} visible)", marker in text))
    for label, ok in verdicts:
        print(f"  {'PASS' if ok else 'FAIL'}  {label}")
    return 0 if all(ok for _, ok in verdicts) else 1


if __name__ == "__main__":
    sys.exit(main())
