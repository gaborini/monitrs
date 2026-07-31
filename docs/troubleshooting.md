# Troubleshooting

Answers to the things that look like bugs and usually are not.

This file explains **why monitrs shows what it shows**.
[`metrics.md`](metrics.md) defines what each number *means*, and every answer below
points back to it rather than repeating it. If a number disagrees with another tool,
read the definition first: a definitional difference is the usual cause, and an
*undocumented* difference is the actual bug.

## Two MEM percentages that look like they disagree

The header meter says `MEM 29%` and the Pressure Radar says `MEM ... 71%`. Both are
correct, and they are not the same quantity.

* The **header meter** shows memory **used** — `29%` of 48 GiB is the ~14 GiB the
  `14G/48G` beside it reports.
* The **radar** shows memory **available**, because that is the number the pressure
  threshold is defined on: `diagnostics.memory_watch_available_percent` fires when
  *available* memory falls, which is what predicts reclaim pressure. 29% used and
  71% available are the same measurement from opposite ends.

The Overview's radar column is too narrow to spell out `available`, which is what
makes the pair read as a contradiction. Press `6` for **Inspect**, where each signal
is shown with the full rule text that produced it — there the memory row states the
threshold in terms of available memory explicitly.

See [`metrics.md`](metrics.md) for why *available* rather than *free* is the number
that matters on both platforms.

## A metric says `permission denied`

You are seeing the `!` symbol and the text `permission denied`. That is a *fact
about your privileges*, not an error and not a zero.

monitrs runs unprivileged by design, never escalates, and never invokes `sudo` or
any external command. When the kernel refuses a read, the affected metric — and only
that metric — reports the refusal.

What is typically refused:

* **Linux:** `/proc/<pid>/io` for a process you do not own, which is where the
  per-process I/O columns come from. Working directories and open-file counts for
  other users' processes go the same way. Kernel threads refuse I/O counters to
  everyone, including root.
* **macOS:** per-process counters for processes belonging to another user. The
  collector reports them as refused rather than as the zeroes the underlying API
  hands back — a zero would be indistinguishable from an idle process.

What to do:

* Nothing, if you do not need that column. Everything else on screen is unaffected.
* Look at your own processes, which are always readable.
* Run as root **only if** you accept the trade. Root adds per-process I/O, working
  directories, and open-file counts for other users' processes. It changes nothing
  else.

`permission denied` (`!`) and `n/a` (`-`) are deliberately different states: root can
grant the first and cannot conjure the second. If a metric says `n/a`, elevated
privileges will not help — the platform does not have the concept.

See [`platform-support.md`](platform-support.md) for the privilege model and the
per-metric support table.

## Network shows a throughput but no percentage

Expected on Wi-Fi and inside virtual machines. A utilization percentage needs a
**link speed**, and the link speed is frequently unreported. A percentage of an
unknown capacity is meaningless, so monitrs shows the throughput — which it does
know — and marks utilization unavailable with the reason `link speed unknown` (`?`).

Where the link speed *is* known, the percentage uses the busier direction, and it is
not clamped at 100%: reported link speeds are often wrong, and clamping would hide
that rather than reveal it.

There is also no per-process network column, on any platform. Doing it correctly
needs packet capture or eBPF, which are out of scope for v1, and a plausible-looking
wrong number is worse than an absent one.

See [`metrics.md` § Network](metrics.md#network).

## Memory `used` disagrees with `free`, `htop`, or Activity Monitor

Because "used memory" is not one definition, and monitrs records which one produced
each snapshot.

**On Linux**, monitrs reports `used = MemTotal - MemAvailable`, and reports page
cache and buffers separately as detail — not as application memory. This is the same
convention as `free`'s `used` column in recent procps versions. If you are comparing
against something that adds cache into "used", monitrs will look *lower*, and monitrs
is describing memory you can actually get back by allocating.

A healthy Linux system deliberately fills memory with cache. Calling all non-free
memory "used" is the single commonest way a monitor misleads its user.

**On macOS**, `available = free + inactive + purgeable` and `used = total -
available`, with **wired** and **compressed** pages reported separately. Activity
Monitor's "Memory Used" is a different composition again, and its "Memory Pressure"
graph is not a number monitrs has access to. Compressed memory is the figure worth
watching: pages held in the compressor are in use in a way Linux page cache is not.

macOS and Linux memory figures are **not byte-for-byte comparable**, and monitrs does
not pretend otherwise. The Inspect screen names the definition in force.

Two further reasons for a legitimate difference:

* **Inside a container**, the applicable ceiling is the cgroup limit, not the host
  total. monitrs shows both, labelled. A tool that shows only the host total will
  disagree with monitrs and be less useful.
* **Sampling instants differ.** Two tools sampling a second apart on a busy machine
  will not agree to the megabyte, and neither is wrong.

See [`metrics.md` § Memory](metrics.md#memory).

## A process shows more than 100% CPU

Working as intended. The default normalization is **one core = 100%**, so a process
using four cores fully reads `400%`. This matches `top` and `htop`.

Set `process_cpu_normalization = "machine"` in your configuration if you prefer the
whole machine to be 100%; the same process then reads `50%` on an 8-CPU box. Core
normalization is the default because it preserves information — `400%` makes the
thread count visible in a way `50%` does not. The convention in force is shown in
the `?` help so it is never ambiguous on screen.

Consequences that surprise people:

* Per-process percentages do **not** sum to the system CPU percentage. System CPU is
  `0..=100` for the whole machine; process CPU is per core.
* A single-threaded process cannot exceed 100%. If one does, that is worth an issue.

See [`metrics.md` § Process CPU](metrics.md#process-cpu) and
[`configuration.md`](configuration.md).

## The first frame says `warming up`

Every metric derived from a difference between two readings — CPU percentages, all
throughput and packet rates, per-process I/O — needs two samples. On the first there
is nothing to subtract from.

`warming up` (`.`) is what that state is called, and it is emphatically **not zero**.
An idle process at `0.0%` and a process nobody has measured yet are different facts,
and a monitor that renders both as `0` is lying about one of them.

It clears on the next sample: one second at the default interval. The same state
appears again, briefly, for:

* a newly discovered process;
* a process whose PID was **reused** — a different process behind the same PID
  starts warming up, because inheriting the old one's baseline would produce a
  fabricated spike;
* a counter that went backwards, which is reported as `counter reset` for that one
  sample while the baseline is re-established.

If a metric is *still* warming up after several seconds, that is worth an issue — it
means the second sample is not arriving or the baseline is being discarded every
time.

See [`metrics.md` § Time and rates](metrics.md#time-and-rates).

## The terminal beeped, or a `pressure` line appeared on its own

A Pressure Radar signal crossed a threshold. The notice names the signal, the state
it reached, and the rule that produced it:

```
X pressure CPU is now critical (held 1s): cpu busy at or above diagnostics.cpu_wa...
```

It is written once per **transition**, not once per sample: the radar reports
`critical` every second for as long as the condition lasts, and only the change is
worth saying. `held` is how long the signal has been in the state it just entered —
at the moment of the transition that is one sample interval, because the engine's
timer restarts when the state changes. The full rule, the raw metric and the
severity bar are all on the Overview's radar panel; the notice is the part you do not
have to be looking at.

Recovery produces one more line, at a lower severity. A signal going *unavailable*
produces none, deliberately: a refused read is not good news.

The bell is off unless `diagnostics.bell_on_critical = true`, and even then it rings
only for an escalation into `critical` — never for `watch`, never on recovery, and
once per episode. If monitrs is beeping more than that, or beeping with the key unset,
that is a bug worth an issue. To silence it, set the key back to `false` and
`reload config` (`:`) — no restart needed. See
[`configuration.md`](configuration.md#being-told-instead-of-watching).

If a signal escalates and clears repeatedly, the condition is genuinely oscillating
around your threshold; raise `diagnostics.sustained_samples` rather than the
threshold, since that is the knob that decides how much agreement a transition needs.

## A value has a `~` and an age beside it

The `~` marker means **retained, not current**: monitrs could not refresh that metric
this sample, so it is showing the last good value together with how old it is. `~ 71%
(4s)` is a four-second-old reading, not a current one.

A stale value is also invisible to every calculation. It cannot become a rate
baseline and cannot feed a pressure rule, because the type that carries it has no way
to hand out the value without its age.

Two things worth knowing:

* **The live collectors do not currently mark anything stale.** The state exists,
  the rendering is snapshot-tested, and the fake collector produces it on demand —
  but no code path in the Linux or macOS collector retains a previous value today.
  If you see a `~` in a live run, please open an issue: either a collector gained
  the behaviour without this note being updated, or something is wrong.
* **Stale is not the same as history.** A paused or seeked timeline is not showing
  stale values; it is showing an earlier sample, and it says so unmistakably. Process
  actions are disabled there, deliberately (§2.1) — the PID on screen may already
  belong to something else.

## A filesystem shows `n/a` instead of a usage percentage

A percentage of zero capacity is undefined, so a mount whose reported total size is
zero gets `n/a` (`-`) rather than `0%` or `100%`. Pseudo-filesystems do this
routinely.

Related things that are *not* inconsistencies:

* **`available` is smaller than `total - used`.** Reserved blocks. Expected on ext4
  and friends.
* **Virtual mounts are hidden by default** (`tmpfs`, `devfs`, `overlay`). On a
  container host they otherwise dominate the list. They are classified, not deleted.
* **A filesystem being full says nothing about the device being busy**, and monitrs
  never combines the two into one percentage. A 95%-full filesystem is not slow; a
  saturated device may sit on a nearly empty one.
* **Device busy percentage is `n/a` on macOS.** It comes from `/proc/diskstats`
  field 10 on Linux and there is no documented macOS equivalent. Deriving it from
  throughput would be meaningless on an NVMe device, which can be "100% busy" at 2%
  of its capability.

See [`metrics.md` § Filesystems and disks](metrics.md#filesystems-and-disks).

## The Battery screen says there is no battery

Because there is not one. A desktop, a server, a virtual machine, a container and
every CI runner reach the Battery screen (`7`) with nothing to report, and the screen
says so in words rather than showing an empty panel or a charge of `0%`. Nothing is
broken and no privilege would change it.

The three absences are different and the screen distinguishes them:

* **`n/a`** — this machine has no battery. On Linux, `/sys/class/power_supply`
  contains no entry whose `type` is `Battery`; on macOS, `IOPowerSources` lists no
  internal battery.
* **`warming up`** — the battery shares the sensor group's schedule with
  temperatures: every 30 seconds ordinarily, tightening to every 5 seconds while
  this Battery screen is the one open. Opening the screen clears the deadline, so
  the first reading lands on the very next tick rather than up to 30 seconds
  later. Once it has landed, a reading between ticks is shown carried over and
  aged (`Stale { value, age }`) rather than dropped back to `warming up`.
* **`permission denied`** — a power source is present but unreadable at this
  privilege level. Rare; worth reporting if you see it.

Related things that are *not* faults:

* **On macOS the cycle count, capacities, pack temperature and watts are all
  `n/a`.** `IOPowerSources` publishes the charge, the state, and the time-remaining
  estimate and nothing else. The rest lives in the `AppleSmartBattery` I/O Registry
  node under property names Apple has never documented, and §9.3 does not permit
  reading those. The figures are obtainable; they are not obtainable *honestly*.
* **There is no time remaining on some Linux laptops.** Most ACPI batteries publish
  no estimate, and monitrs will not compute one: charge divided by an instantaneous
  current swings by hours between consecutive samples, which is precisely the kind of
  invented number §4 exists to prevent.
* **A cycle count of `n/a` on a machine that has cycled its battery.** Firmware that
  does not count cycles reports `0`, which monitrs treats as unknown — "0 cycles" on
  a four-year-old laptop would be a fabricated all-clear about the pack's age.
* **`0.0 W` on a full pack on mains is a measurement**, not an absence. A pack that
  is neither charging nor discharging really draws nothing.
* **A thermal sensor with no bar beside it.** A bar needs a full scale and a
  temperature has none unless the sensor declares a threshold, so the bar appears
  only where it does. Same rule as network utilization without a link speed.

See [`metrics.md` § Sensors and battery](metrics.md#sensors-and-battery).

## The terminal is too small

monitrs has four layout bands, and it degrades rather than breaking:

| Terminal size | What you get |
|---|---|
| ≥ 140 × 38 | Wide: every panel |
| ≥ 100 × 28 | Standard |
| ≥ 80 × 20 | Compact |
| ≥ 60 × 16 | A header and a stable process list, nothing else |
| below 60 × 16 | Three lines of text, and nothing else fits |

Below 60 × 16 you get exactly this:

```text
monitrs needs at least 60x16
current terminal: 52x12
resize or press q to quit
```

What to do: resize the window, reduce the font size, or leave the pane. It recovers
the moment the terminal grows — resizing is handled as an event, not sampled, so
there is nothing to wait for and nothing to restart.

Two design notes that explain what you see while dragging a window:

* The 60 × 16 band shows a header and a process list and *nothing that could appear
  and disappear* as you drag, because a layout that flickers between bands is worse
  than a small one.
* Column widths come from the panel geometry, never from the current values, so a
  number crossing a unit boundary — `999M` to `1.0G` — never reflows the table.

No calculated rectangle can be negative or oversized, and a zero-area panel renders
nothing rather than panicking; that is pinned by a property test over every terminal
size up to 400 × 400.

## Only a handful of processes are listed

Usually correct rather than broken:

* **In a container**, process visibility is whatever the PID namespace allows. Three
  processes is a true answer.
* **Kernel threads are hidden by default on Linux.** They are real, they are just
  noise for most questions.
* **A filter is active.** Clear it before concluding anything is missing.
* **A process can vanish between any two reads.** That is normal, is never logged as
  an error, and is why a row is keyed on `(PID, start time)` rather than on the PID
  alone.

See [`platform-support.md` § Containers and virtual machines](platform-support.md#containers-and-virtual-machines).

## monitrs itself is using CPU or memory

It should be measurable and small: §16.1 budgets under 1% median idle CPU and under
50 MiB resident in the default configuration. A monitor is obliged to expose its own
overhead, and monitrs measures it — see [`soak-testing.md`](soak-testing.md) for the
harness and the recorded figures.

If yours is over budget, the interval is the first thing to check. `--interval 250ms`
is four times the work of the default, and a 10,000-process machine at 250 ms is a
different proposition from a laptop at one second. `--history` costs memory in
proportion to its span; both are clamped to supported ranges and any clamp is
reported rather than applied silently.

Both are measured now, and one of them **misses half its budget**: on a 12-core Mac with
about a thousand processes, monitrs costs a median 0.5–1.1% of one core at rest against a
1% target — which passes — but a p95 of 6–11% against 2%, which does not. Resident memory
sits at 24.5–26.7 MiB against 50 MiB and a frame renders in 200 µs against 16 ms. If your machine runs many processes, expect the CPU figure
rather than the budget:
[`benchmarks.md`](benchmarks.md#where-the-idle-cpu-goes) breaks the cost down read by
read — most of it is the OS handing over the process table and the disk counters, not
anything monitrs computes.

`scripts/measure-overhead.py` takes the same measurement on your machine, from
outside the process, if you want to compare.

## A number looks wrong

Please report it, and please use the **Incorrect metric** issue template rather than
the bug template: a wrong number needs different evidence from a program that
misbehaves. It asks for the metric, what monitrs said, what you believe is correct,
and how you checked — that last one being the part that makes the report actionable.

Include `monitrs --version`, your OS version and architecture, and — if you can — a
`monitrs snapshot --format json` capture. The snapshot redacts process arguments by
default, and never contains environment variable values, because monitrs does not
read them at all. Check the output before attaching it anyway; a command line can
contain a credential.

For a suspected security issue, do not open an issue at all — see
[`SECURITY.md`](../SECURITY.md).
