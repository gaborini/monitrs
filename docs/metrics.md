# Metric semantics

Every number monitrs shows is defined here. If a value disagrees with another
tool, the cause is usually a definitional difference rather than a bug — and if
that difference is not documented below, that *is* the bug.

Two rules apply to everything on this page:

* **Unavailable is not zero.** A metric monitrs could not measure renders as
  `warming up`, `permission denied`, `n/a`, or a named transient reason. It never
  renders as `0`.
* **Stale is not current.** A retained value is marked and shows its age. It is
  also invisible to calculations, so a stale reading can never become a rate
  baseline or feed a pressure rule.

## Availability states

| Rendered as | Symbol | Meaning |
|---|---|---|
| `warming up` | `.` | Needs a second sample. The first sample of any delta-based metric is *not* zero. |
| `permission denied` | `!` | The OS refused the read at our privilege level. Elevated privileges would help. |
| `n/a` | `-` | This platform has no such concept. Root cannot conjure it. |
| named reason | `?` | Normally available, absent from this sample — e.g. `counter reset`, `device disappeared`, `link speed unknown`. |
| value with marker | `~` | A retained previous value, shown with its age. |

## Time and rates

Each snapshot carries a monotonic `Instant` for arithmetic and ordering, a
`SystemTime` for display, and the **actual** elapsed interval since the previous
snapshot.

```
rate = (current_counter - previous_counter) / actual_elapsed_seconds
```

The interval is never assumed to be one second. Suspend/resume, load, and
scheduler delay all vary it, and a laptop resumed after an hour would otherwise
report a rate thousands of times too large.

Handled explicitly:

* **First sample** → `warming up`.
* **Counter went backwards** → `counter reset` for that sample, and the baseline
  is re-established so the *next* sample is valid. Never a huge or negative rate.
* **Wraparound** → when the counter width is known (e.g. a 32-bit interface
  counter), a backwards move consistent with a single wrap is computed as a
  wrapped delta. When the width is unknown, it is treated as a reset, because
  guessing would fabricate traffic.
* **Device or interface disappears and returns** → the old baseline is discarded.
  A network interface renamed mid-run invalidates its baseline too.
* **A wall-clock jump** cannot produce a negative rate or make history move
  backwards, because ordering never consults the wall clock.

## CPU

### System CPU

Aggregate machine utilization, `0..=100`. Computed from the delta of CPU time
counters over the actual elapsed interval, not from an instantaneous reading.

Where the platform exposes the split, the breakdown is user, system, nice, and
idle, plus — **Linux only** — iowait, irq, softirq, and steal. On macOS those four
are `n/a`, not zero. `steal` is the most useful of them: sustained non-zero steal
means the hypervisor is not giving this VM the CPU it was scheduled.

### Process CPU

**Default: one core = 100%.** A process fully using four cores reads `400%`. This
matches `top` and `htop`.

The alternative is `process_cpu_normalization = "machine"`, where the whole
machine is 100% and the same process reads `50%` on 8 CPUs. Core normalization is
the default because it preserves information: `400%` makes the thread count
visible in a way `50%` does not.

The active convention is shown in `?` help so it is never ambiguous on screen.

A process's first sample is `warming up`. Percentages are computed from CPU-time
deltas and the logical CPU count, using the real elapsed interval.

### Load average

Load is a **run-queue length, not a percentage**, and monitrs never renders it as
one. The comparable form is load per logical CPU: `11.4` on 8 CPUs is `1.43` per
CPU, which is what the load pressure rule uses. On a machine whose CPU count is
unknown, load-per-CPU is unavailable rather than assumed.

## Memory

Linux and macOS do not mean the same thing by "used memory". monitrs records which
definition produced each snapshot's headline numbers and shows it on the Inspect
screen.

### Linux

| Metric | Definition |
|---|---|
| total | `MemTotal` |
| available | `MemAvailable` — the kernel's own estimate of what can be allocated without swapping |
| used | `total - MemAvailable` |
| free | `MemFree`, completely unused. Usually far smaller than `available`. |

Page cache and buffers are reported as detail and are **not** counted as
application use. Labelling all non-free memory as application memory is the single
most common way a monitor misleads its user: a healthy Linux system deliberately
fills memory with cache.

### macOS

| Metric | Definition |
|---|---|
| total | physical memory |
| available | free + inactive + purgeable pages |
| used | `total - available` |

**Wired** pages cannot be paged out. **Compressed** pages are held in the memory
compressor. Both are reported separately, because neither is reclaimable the way
Linux page cache is — a machine with substantial compressed memory is under real
pressure even though the pages are "in use" in a different sense.

macOS memory figures are **not byte-for-byte comparable** with Linux figures.
monitrs does not pretend otherwise.

### Both platforms

* **RSS (resident set size)** — physical memory currently mapped in. The number
  to look at.
* **Virtual size** — address space reserved, most of which is typically never
  resident. It is the lowest-priority column on purpose: it is the most
  frequently misread number in any process table. A process with a 400 GiB
  virtual size and a 50 MiB RSS is normal.
* **Swap** — capacity and usage, plus swap-in and swap-out **rates** where
  available. The rates are what indicate distress. A large idle swap file is
  unremarkable; sustained swap-in on a small one is not. Swap disabled is reported
  as `0 of 0` — a fact, not an unavailable metric — while a *percentage* of zero
  capacity is genuinely undefined and shown as such.
* **cgroup limits** — inside a container the applicable ceiling is the cgroup
  limit, not the host total. Both are shown and labelled where observable. An
  "unlimited" cgroup sentinel is recognised and does not shrink the ceiling.

## Filesystems and disks

**These are different metrics and are never combined into one percentage.** A
filesystem 95% full is not busy. A device saturated at 100% utilization may sit on
a nearly empty filesystem.

### Filesystem capacity

Total, used, available, and usage percentage per mount point, plus the filesystem
type and whether it is read-only. `available` is normally smaller than
`total - used` because of reserved blocks — this is expected, not an
inconsistency.

Mounts are classified as physical, removable, network, or virtual. Virtual mounts
(`tmpfs`, `devfs`, `overlay`) are hidden by default because on a container host
they otherwise dominate the list.

Capacity lives in the medium sampling tier because `statfs` on a stalled network
mount can block for seconds.

### Device throughput

Read and write throughput, read and write operations per second, and cumulative
counters, per block device.

**Busy percentage** — the share of wall time the device had at least one request
in flight — is shown **only where it is semantically correct**. That means
`/proc/diskstats` field 10 on Linux, and `n/a` elsewhere. A queue-depth-derived
approximation on an NVMe device that services requests in parallel is not merely
imprecise, it is meaningless: such a device can be "100% busy" at 2% of its
capability.

Mapping devices to mount points is expensive and lives in the on-demand tier, so
it may be absent in a fast-tier snapshot.

## Network

Per interface: operational state, addresses, receive and transmit throughput,
packets per second, and error and drop counters where available.

**Utilization is unavailable without a known link speed.** A percentage of an
unknown capacity is meaningless, so monitrs shows throughput only — this is the
`? NET unknown 18M/s` row in the default layout. Link speed is frequently
unreported on Wi-Fi and in virtual machines.

Where the link speed *is* known, utilization uses the busier direction, since a
duplex link saturates in whichever direction fills first. It is deliberately not
clamped at 100%: reported link speeds are often wrong (aggregated links, stale
Wi-Fi negotiation), and clamping would hide that rather than reveal it.

Two totals are shown, and they are different things:

* **Since launch** — accumulated by monitrs. Always meaningful, starts at zero.
* **OS counter** — the kernel's own total. Larger and more useful, but may have
  wrapped or been reset.

**Per-process network attribution is not implemented.** Doing it correctly needs
packet capture or eBPF, both of which are out of scope for v1. monitrs shows no
per-process network column rather than a plausible-looking wrong one.

## Pressure

Each pressure signal shows four things: the raw metric, a normalized `0..=100`
severity, **the rule that produced the state**, and an explicit unavailable state.
The rule text is data, not documentation, so the answer to "why is this amber?" is
always on screen.

States are `normal` (`.`), `watch` (`!`), and `critical` (`X`). Color reinforces
the state but is never the only indicator, and an unmeasurable signal reads as `?`
rather than as healthy — a system whose pressure cannot be measured is not known
to be fine.

Severity is separate from state because two signals can both be `watch` while one
is far closer to critical.

Rules use **hysteresis**: a signal must sustain a condition for a configured
number of samples before escalating, and must clear it before de-escalating. This
is what stops a monitor flapping between amber and green once per second. Rules
also reset cleanly after a counter reset or a sleep/wake cycle rather than
treating the discontinuity as an event.

### Linux PSI

Where `/proc/pressure/*` exists, PSI is reported directly. `some` is the share of
time at least one task was stalled on the resource; `full` is the share where
every runnable task was stalled. `full` is absent for CPU on many kernels and is
shown as `n/a` there.

PSI is the best available signal for memory and I/O pressure because it measures
the stall itself rather than inferring it from a utilization number.

## History and attribution coverage

Default retention is 300 samples at one second: five minutes. Configurable within
250 ms–60 s interval and 30 s–60 min history, bounded additionally by a memory
budget. When configuration exceeds either bound it is clamped and the clamping is
reported, so monitrs never silently does something other than what was asked.

Each sample retains a compact system aggregate plus the top 10 contributors for
CPU, resident memory, disk read, and disk write — **not** the full process table.
Each contributor keeps its stable identity, name, a truncated command, the
absolute measurement, and the delta or rate.

**Evidence coverage** is the share of the observed system total that the retained
contributors account for. It is displayed with every attribution:

```
Evidence: 78% of observed CPU accounted for by retained top processes.
```

That number is the honest limit of the feature. If coverage is 78%, then 22% of
the observed CPU was spread across processes that did not make the top ten, and
the spike may not be explained by what is on screen at all.

Attribution is **sample correlation**, not causation. monitrs says "top
contributors" and "correlated with the spike", and it will not tell you a process
caused a system event unless the collected data actually proves it. A counter reset
never becomes a spike: an unavailable input stays unavailable in history.

## What monitrs will not diagnose

Diagnostic rules are deterministic evaluations over collected evidence, marked
with a confidence of low, medium, or high. They will not conclude OOM, a memory
leak, disk failure, malware, or thermal throttling from a single ambiguous metric.

Temperature is a case worth naming: monitrs reports what the sensor says, including
whether the sensor's *own* declared critical threshold has been reached. It draws
no throttling conclusion from that, because a temperature reading alone does not
establish one.
