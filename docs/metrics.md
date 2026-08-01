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

### Inside a container

The host's CPU count is still the host's. A group limited to 1.5 CPUs on a 64-CPU machine
is not "2% of the machine"; it is a hard wall its processes are throttled against. monitrs
reads `cpu.max` and shows the ceiling beside the count, never instead of it:

```
cpu    8 logical, 8 physical, cgroup 1.5 CPUs
```

The header carries the same thing as `cgroup 1.5 cpu`, ahead of the temperature, and the
Inspect screen names the container itself where the cgroup path identifies one —
`container docker 3f4a1b2c9d8e`. An unlimited group is *unsupported*, not a very large
number of CPUs; a `cpu.max` that cannot be parsed is *unavailable*, not "no limit", because
those two mean opposite things to someone trying to explain a stall.

A configured limit counts even when the read that found it is stale. A limit is
configuration rather than a measurement: if the last good read said 1.5 CPUs and this
tick's failed, the group is still limited to 1.5, and falling back to the host's 64 would
advertise headroom that does not exist.

**The load average is not scoped to the container, and monitrs does not pretend it is.**
`/proc/loadavg` is not namespaced either: inside a container it counts every runnable
process on the machine, including other tenants'. Dividing that by this group's quota would
pair a host numerator with a container denominator and produce a figure describing nothing
— the same mistake as dividing host `used` by a cgroup limit. So the divisor stays the
host's CPU count, and where a quota exists the label says `over 8 host cores` to make clear
which machine the figure is about. The group's own saturation is cgroup PSI, a different
measurement, and inventing it by division would be worse than not having it.

## Open files and sockets

Three separate figures, all on the on-demand tier and all shown in the process
detail overlay (`Enter` on a process):

* **`OPEN FILES`** — how many descriptors the process holds. Counted by enumerating
  the descriptor table, not read from a field that reports the table's *size*:
  macOS' `pbi_nfiles` looks exactly like this number and is not one, because it does
  not move when files open and close.
* **`SOCKETS`** — how many of those descriptors are sockets. **macOS only.** One
  `PROC_PIDLISTFDS` returns a type for every descriptor, so the count is free.
  `/proc/<pid>/fd` gives no such thing: a descriptor's type is only visible once its
  link target has been read, one syscall at a time, so a Linux socket count taken
  from the capped walk below would be a floor pretending to be a total. On Linux the
  count is `n/a`, and the sockets that appear in the list are still labelled as
  sockets.
* **`DESCRIPTORS`** — how much of the list is on screen, as `12 of 12 listed` or
  `256 of 4096 listed, 3840 not listed`. The list stops at 256 descriptors, because
  naming one costs a syscall on both platforms. The row is always present, so a
  refused table reads `permission denied` and a platform that cannot list descriptors
  reads `n/a` — never an empty list, which would say the process holds nothing open.

Each listed row is `FD <n> <kind> PATH <path>`. The kind is there because not every
descriptor has a path: a socket, a pipe, and an event queue have none at all, so
their path is `n/a` and the kind is what says why rather than leaving a blank that
reads as a failed read. A path the OS refused is `permission denied`. A path is never
an empty string.

Paths are user data. They are shown on screen and go nowhere else — not into the
debug log, and not into a JSON export, which carries no process detail at all and
could not carry a path if it did (§15.2, §19).

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
* **cgroup usage** — and the counterpart nobody expects to need: `used` above is the
  **host's**, because `/proc/meminfo` is not namespaced. A process in a 2 GiB group on a
  64 GiB host reads the host's 40 GiB and concludes it is nearly out of memory when it has
  used 300 MiB of its own allowance. monitrs reads the group's own charge from
  `memory.current` — the counter the kernel compares against `memory.max` when it decides
  to OOM-kill — and shows it beside the limit it is enforced against:
  `cgroup limit 2.0G, 512M used (25%)`. Both halves of that ratio come from the group, or
  neither does; the host figure is never divided by a container limit.

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

**Two mounts can share one device, and then their sizes are not additive.** On an
APFS Mac `/` and `/System/Volumes/Data` both report the whole container — the same
494G twice, not 988G of disk. The Storage screen marks rows whose device backs
another visible mount with `=` and says so in the panel's label. The numbers
themselves are correct as reported; what the mark prevents is adding them up.

### Inodes

`INODE%` and `IFREE`: how much of the filesystem's inode table is in use, and how
many entries are left. This is a **different exhaustion** from running out of bytes
and it is invisible in the byte columns — a filesystem with 200 GB free can refuse
to create a file because the table is full, and `USE%` will read a comfortable 40%
while it happens.

Read from `f_files` and `f_ffree`: `getfsstat` on macOS, `statfs(2)` per mount on
Linux. `sysinfo` does not expose either field, so on a build without the native
layer these columns read `n/a`.

Many filesystems have no fixed inode table — including several pseudo-filesystems
and any filesystem that allocates entries dynamically. Those report `f_files == 0`,
which monitrs renders as `n/a` and **never** as `0 of 0`: "no inode limit" and "no
inodes left" are opposite claims, and only the first one is true. A mount whose
`statfs` was refused reads `denied`, which is a third and distinct answer.

Inodes are a medium-tier read, beside the byte capacity and for the same reason.

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

### Per-process disk I/O

The Storage screen ranks processes by read plus write throughput, with each
process's cumulative `TOTAL R` and `TOTAL W` beside the rates. The totals are the
*process's own* counters — bytes since it started, as `/proc/<pid>/io` and
`proc_pidinfo` report them — and not something monitrs accumulated; a process that
restarts starts them again.

Two ordering details worth knowing, because both are visible:

* A process whose counters the OS refused sorts **below** a process measured as
  idle. `denied` is not zero, and a refused row must not push a measured one off
  the panel.
* Where the rates tie — which on a real machine is most processes most seconds —
  the order falls back to the cumulative totals. Ordering thirty idle rows by PID
  answers nothing; ordering them by what they have written names the heavy users of
  the disk.

Kernel threads are excluded from the ranking: their per-process counters are
refused on both platforms, and the question the panel answers is which
*application* is using the disk.

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

## Sensors and battery

Both are optional on every platform, and both are read as one **sensor group** on a
cadence of its own rather than on the medium tier: **every 30 s**, or every 5 s while
the Battery screen is visible, and immediately when that screen is opened (§8.6). A
pack's charge moves in whole percentage points over minutes, and reading it every
second would be a dozen file opens to watch a number that has not changed — while on
macOS the temperature read alone blocks for about 85 ms, which is why it is not on the
5-second schedule the rest of the secondary metrics share.

The consequence on screen: between reads the last value is carried forward and shown
**with its age** rather than dropped, so a temperature or a charge level can be up to
30 seconds old and says so (`62.5C`, panel label `2 sensors ~00:28`). A retained
reading is never presented as a fresh one.

### A machine with no battery

**A desktop, a server, a virtual machine, a container and a CI runner all have no
battery, and that reads as `n/a` with a reason — never `0%`.** This is a fact about
the hardware, not a failed read, and the Battery screen says so in words rather than
leaving a blank panel. It is the most common reading of the whole screen.

The states are distinguished, because they call for different responses:

| Reading | Meaning |
|---|---|
| `n/a` | This machine has no battery. Nothing to fix. |
| `warming up` | The sensor group has not read `/sys/class/power_supply` yet. Opening the Battery screen asks for a read straight away, so this is normally gone within a tick or two. |
| `permission denied` | A power source is present but unreadable at this privilege level. |

### Battery fields

| Field | Definition |
|---|---|
| charge | Charge level, `0..=100`. On Linux the kernel's own `capacity`; on macOS `Current Capacity / Max Capacity` from `IOPowerSources`. |
| state | `charging`, `discharging`, `full`, `not charging`, `unknown`. `not charging` is real and distinct from `full`: it is what macOS optimised charging and Linux charge thresholds look like, and calling it "full" would misreport a pack deliberately held at 80%. |
| time remaining | **Only what the platform published.** Time to empty while discharging, to full while charging. Absent on any pack whose OS publishes no estimate. |
| cycle count | Completed charge cycles. A reported `0` or `4294967295` is treated as *unknown*, because ACPI firmware that does not count cycles reports exactly those, and "0 cycles" on a four-year-old laptop is a fabricated all-clear. |
| capacity | Design capacity beside today's full-charge capacity, in watt-hours. The second number against the first is what tells you the pack is worn. |
| health | `full / design`, derived from the pair above and stored nowhere, so it can never disagree with the two figures printed beside it. Not clamped to 100%: a new cell often measures above its design capacity. |
| temperature | Pack temperature, where the pack reports one. Distinct from the thermal sensors: a battery at 45 °C means something different from a CPU package at 45 °C. |
| power | Instantaneous power through the pack, in watts, as a **magnitude**. Direction is the charge state's job, because the sign of Linux's `current_now` is driver-dependent. A `0.0 W` reading on a full pack on mains is a measurement, not an absence. |

**No time remaining is ever computed.** A figure derived from one instantaneous
current reading swings by hours between consecutive samples, and it is the number
users trust most — which is exactly why §4 forbids inventing it. Where the platform
publishes no estimate, monitrs shows none.

Two capacity unit systems exist on Linux and only one reaches the model. Drivers
report either energy in µWh (`energy_full_design`) or charge in µAh
(`charge_full_design`); the amp-hour form is converted through
`voltage_min_design`, the nominal pack voltage the same kernel ABI provides. A
driver reporting amp-hours and no nominal voltage leaves the capacity `n/a`: there
is no second source for the missing factor, and multiplying by a plausible 11.4 V
would fabricate a watt-hour figure.

The system battery is picked out of `/sys/class/power_supply` by two documented
attributes — `type` must be `Battery` and `scope`, where present, must be `System`.
That is what excludes the charger, a UPS, and a bluetooth mouse's own cell, which
all appear in the same directory on an ordinary laptop.

### Thermal sensors

Reported as the sensor names them, with whatever thresholds the sensor itself
declares. A reading at or above the sensor's own critical threshold is flagged; see
[What monitrs will not diagnose](#what-monitrs-will-not-diagnose) for why that is as
far as it goes.

**No temperature bar is drawn without a threshold to scale it against.** A bar needs
a full scale and a temperature has none — 62 °C is most of the way to a laptop's
limit and barely warm for a GPU — so the bar appears only where the sensor declares
a ceiling, and a sensor that declares none shows the figure and says why there is no
bar. This is the same rule that forbids a network utilization percentage without a
known link speed.

## Following a process with its children

`F` on a process row scopes the table to that process and everything beneath it; `F`
again lifts the scope. The palette has `follow [pid]` and `unfollow` for the same thing
(§6.3). The panel title says `following 410` while the scope is on, because a table showing
four rows out of a thousand with nothing on screen to explain it is indistinguishable from
a monitor that has lost the other processes.

The reason to follow rather than to filter is the figure in the trailing label:

```
+ PROCESSES  sort CPU% desc  following 410 --------- 4 of 10 total, cpu >=107%, rss 479M -+
```

`cpu 107%` is what the family costs together, and it is on no row. A build's compilers come
and go every second; the individual rows never answer "what is this build using".

### The sums, and their limits

* **`>=` means the sum is a lower bound.** A member whose CPU the OS refuses — a
  compiler running as another user, say — is still counted as a member and still shown in
  the table with `denied` in its CPU cell, but it cannot contribute to the total. The
  marker is there so that three compilers out of four are not presented as the whole
  family's cost. A sum with *no* contributors is not zero: it reports the members' own
  state, so an all-refused family reads `denied` rather than `0%`.
* **A stale member still contributes.** It was measured; the row is marked stale anyway.
  Excluding it would make the family's total drop whenever one read failed once, which
  looks like the build getting cheaper.
* **`rss` over-counts shared pages.** Two compilers sharing 100 MiB of mapped libraries
  contribute that 100 MiB twice, which is a property of RSS rather than of the sum — the
  same property every per-process `RSS` column on every screen has. It is labelled `rss`,
  not "memory used", for that reason. Neither platform gives monitrs a per-process
  unique-memory figure cheaply enough to sample every second, so there is no honest
  alternative number to show here.
* **`subtree of N` appears only when it differs from the row count.** The sums always
  cover the whole family, so when a text filter narrows the view to two rows, the label
  says what the 107% is actually over.

### What a subtree is, and is not

Membership is **downwards only**: the followed process and its descendants, never its
parent. Following `make` does not drag in the shell that launched it, or the terminal, or
the login session.

The root's own identity is a `(pid, start_key)` pair, not a PID, so a recycled PID cannot
quietly become the thing being followed. When the root exits, monitrs **stops following**
and says so:

```
stopped following 410: the process has exited
```

It does not keep following the orphans. The kernel reparents them to init, and a family
whose common ancestor is gone is not the family that was asked for. Losing only *children*
changes nothing — a build's membership changes constantly, and releasing the scope on every
change would make the feature useless.

Cycles in the parent links terminate and are counted rather than followed, as the tree view
already does.

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
