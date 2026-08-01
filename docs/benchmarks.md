# Benchmark results

§16.3 requires that optimisation is not guided by intuition, and that baseline
results are stored — or at minimum that the reference machine and command are
documented. This file is that record.

Two kinds of measurement live here: the microbenchmarks of specific functions
(`cargo bench`), and the end-to-end §16.1 budgets, which are measured against the
live collector and the assembled renderer. Both sections say what they do not cover.

## Reference machine

| | |
|---|---|
| CPU | Apple M4 Pro, 12 logical / 12 physical cores |
| Memory | 48 GiB |
| OS | macOS 26.5.2 (arm64) |
| Toolchain | rustc 1.97.1 |
| Profile | `bench` (inherits `release`: `lto = "thin"`, `codegen-units = 1`, debug symbols kept) |

This is a fast desktop-class machine. The §16.1 reference workload is a more
modest 8-CPU laptop, so treat these as a lower bound on real-world cost.

## Command

```sh
cargo bench -p monitrs --bench pipeline

# One group at a time, which is what you want while changing something:
cargo bench -p monitrs --bench pipeline -- linux_parsers
```

The end-to-end §16.1 numbers further down come from two different commands, named in
that section: they need the live collector and the assembled renderer, which a
criterion benchmark is the wrong shape for.

The run below used shortened sampling for turnaround:

```sh
cargo bench -p monitrs --bench pipeline -- \
  --warm-up-time 1 --measurement-time 3 --sample-size 20
```

Criterion writes full reports to `target/criterion/`.

## Results

Two workloads: **200** processes (the §16.1 reference) and **10,000** (the §16.2
high-load case). Times are the median of Criterion's confidence interval.

### Rate engine

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `single_counter_observe` | 5.6 ns | — |
| `single_counter_with_resets` | 4.9 ns | — |
| `keyed_observe_per_process` | 1.74 µs | 102 µs |

Counter reset handling is not a slow path — detecting a reset is marginally
*cheaper* than computing a rate, because it skips the division. There is no
performance argument for cutting the §8.2 correctness check.

### History ring

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `record_one_sample` | 14.0 µs | 972 µs |
| `reject_a_re_delivered_sample` | 3.0 ns | — |
| `seek_back_steps` (1 / 10 / 150 / 299) | 507 / 503 / 498 / 506 ps | — |
| `seek_to_offset_150s` | 6.7 ns | — |
| `comparisons_against_30s_ago` | 17.3 ns | — |

Two things worth stating plainly:

**Seeking is measurably constant time.** Stepping back 299 samples costs the same
as stepping back 1 — around 0.5 ns either way. §21's M4 acceptance criterion asks
for "constant or effectively constant time"; this is the evidence, not an
assertion.

**Recording is dominated by contributor extraction**, not by the ring. At 10,000
processes `record_one_sample` (972 µs) is almost exactly
`contributors/top_10_of/10000` (977 µs). The ring itself is free; selecting the
top ten of ten thousand is the cost. That is where to look if history recording
ever needs to be faster.

### Contributor extraction

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `top_10_of` | 13.8 µs | 977 µs |
| `top_10_with_trend` | 13.9 µs | — |

Deriving trends against the previous sample is free within noise (13.78 → 13.90
µs), so the §2.5 comparison values cost nothing worth optimising.

### Process list

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `sort_by_cpu` | 17.0 µs | 1.61 ms |
| `sort_by_name` | 31.7 µs | 755 µs |
| `filter_plain_text` | 1.50 µs | 59.9 µs |
| `filter_no_matches` | 2.18 µs | 109 µs |
| `build_tree` | 10.3 µs | 690 µs |

**Sorting by CPU is slower than sorting by name at 10,000 processes** (1.61 ms vs
755 µs), which inverts the 200-process result. This is not a defect: the synthetic
workload gives its processes only 17 distinct CPU values, so nearly every
comparison ties and falls through to the `(pid, start_key)` tie-breaker that §7.2
requires for stable selection. Names, by contrast, are distinct enough to sort in
one pass.

The finding is real and worth keeping in mind: on a machine where most processes
are idle at 0%, CPU sorting is tie-breaker-bound. It is also the argument against
"optimising" the tie-breaker away — it is what stops rows jumping every tick.

`filter_no_matches` costing more than `filter_plain_text` is expected: a match can
stop at the first field, a non-match must check all of them.

### Formatting

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `format_table_rows` | 73.5 µs | 3.71 ms |
| `format_bytes_at_unit_boundaries` (10 values) | 525 ns | — |

`format_table_rows` formats every column of every process, which is deliberately
pessimistic — a real frame formats only the visible rows, typically 20 to 50. At
that scale the cost is a few microseconds. The 10,000-row figure is the answer to
"what if we formatted everything", and the answer is: don't.

Unit-boundary formatting is ~52 ns per value, so the stable-width guarantee of
§5.4 is not paid for in performance.

### End to end

| Benchmark | 200 | 10,000 |
|---|---:|---:|
| `fake_collector_tick` | 19.6 µs | 1.26 ms |

This is the fake collector, so it measures the snapshot *construction* cost with
no OS reads. It is a floor for the sampling loop, not a prediction of it: the
live `sysinfo` collector's cost is dominated by reading `/proc` or calling
`sysctl`, which this cannot model.

### Graph generation

Both glyph modes, at three widths, drawing one row of a series where every fourth
sample is unavailable — the gap marker is a different path from a bar, and a series
that was entirely available would measure the cheap half only (§4).

| Benchmark | 40 cells | 96 cells | 160 cells |
|---|---:|---:|---:|
| `sparkline_ascii` | 586 ns | 952 ns | 1.37 µs |
| `sparkline_unicode` | 755 ns | 1.40 µs | 2.14 µs |
| `meter_ascii` | 565 ns | 756 ns | 1.03 µs |
| `meter_unicode` | 749 ns | 1.32 µs | 2.01 µs |

Unicode costs about 1.5× ASCII, which is the eighth-block ramp having eight levels
against ASCII's five and a multi-byte character per cell. §5.1 makes ASCII a
first-class mode rather than a fallback, so the interesting number is that neither is
expensive: an Overview draws six of these, about 13 µs, against a measured 200 µs
frame.

### Linux fixture parsing

Per call, against the sanitized `typical` fixture for each file. These run on macOS —
every parser takes bytes rather than a path, so the kernel's cost of *producing*
`/proc` is not measured here and the parser's cost of consuming it is.

| Benchmark | Time | Bytes |
|---|---:|---|
| `loadavg` | 36 ns | one line |
| `pid_io` | 118 ns | per process |
| `pressure` | 144 ns | PSI, one resource |
| `pid_stat` | 393 ns | per process |
| `pid_stat_parens` | 383 ns | per process, name with spaces and brackets |
| `proc_stat` | 481 ns | all cores |
| `pid_status` | 620 ns | per process |
| `net_dev` | 956 ns | all interfaces |
| `diskstats` | 994 ns | all devices |

Two things worth reading off this. The three per-process parsers together are about
1.1 µs, so a thousand processes cost about 1.1 ms of *parsing* per tick — against tens
of milliseconds for the reads that feed them. And `pid_stat_parens` is not slower than
`pid_stat`: the case that cannot be split on whitespace, because a process name may
contain spaces and brackets, takes the scan-from-the-right path and pays nothing for
it. That was an assumption worth checking rather than believing.

### Reducer absorb

What it costs to fold one snapshot into the application state — history record, filter,
sort, selection resync — and, for scale, what that absorb would be delaying.

| Benchmark | 200 processes | 10,000 processes |
|---|---:|---:|
| `apply_snapshot` | 18.6 µs | 2.01 ms |
| `apply_keypress` | 65 ns | — |

This is the number that settles "can a keypress queue behind a snapshot". At the
reference workload the answer is 18.6 µs, and even at ten thousand processes a keypress
can be delayed by at most 2 ms — which is why the 90 ms worst case an early soak reported
could not have been what it was first explained as. Independent measurement against the
*live* collector at 979 processes agreed: median 140 µs, max 228 µs. The claim went
unchecked for as long as it did partly because this benchmark did not exist.

### Diagnostic-rule evaluation

Measured on a *warm* engine. A cold one short-circuits — every tracker answers warming
up until its window fills — so timing that would measure the path the rules do not
take (§11.3).

| Benchmark | 200 processes | 10,000 processes |
|---|---:|---:|
| `observe_one_snapshot` | 144 ns | 147 ns |

The point of the second column is that there is no difference. No rule iterates the
process table, and this shows it rather than asserting it: evaluating the whole radar
costs about the same as parsing one `/proc/pressure` file, whatever the machine is
running.

## The §16.1 end-to-end budgets

Six of the eight are now measured on this machine. §16.1's last line says these are
"engineering budgets, not marketing claims, until measured reproducibly", so here is
the measurement, including the one that fails.

Frame time, input latency and collection come from
`crates/monitrs/tests/capture.rs` (`cargo test -p monitrs --release --test capture
-- --ignored --nocapture`), against the live platform collector on the real
renderer.

Those tests **report** their measurements everywhere and **assert** the §16.1 budgets
only when `MONITRS_REFERENCE_MACHINE=1` is set. That is not a loophole, it is the point
of §16.1's own last line: a shared CI runner is virtualised and co-tenanted, and
asserting a 16 ms frame budget there failed at 17.4 ms on hardware where this machine
measures 0.4 ms, with no defect behind it. Every run still asserts a ceiling of twelve
times the budget, which no scheduler can trip and no real regression can hide behind. Set
the variable when you are taking a measurement you intend to quote. Self CPU, resident memory and file descriptors come from
`scripts/measure-overhead.py`, which drives the release binary on a pty and samples
`ps` and `lsof` from outside — measuring monitrs' own cost with monitrs' own
collector would be measuring the thing with itself.

| §16.1 target | Measured | |
|---|---|---|
| ordinary frame render below 16 ms at 160×48 | median 200 µs, p95 353 µs, max 410 µs | pass, by a factor of 45 |
| input-to-visible-response below 50 ms | median 417 µs, p95 486 µs | pass, by a factor of 100 |
| sample collection below 200 ms p95 | one clean run, 984 processes: fast-only (4 in 5): median 9.26 ms, p95 12.63 ms. Fast+medium (every 5th, sensors excluded — they no longer share this tier, see below): median 36.14 ms, p95 40.90 ms. Every tier (every 30th, and the first, sensors included): median 124.21 ms, p95 134.78 ms. These are wall-clock; for what each tick shape costs in **CPU**, which is a different and in one case very different number, see ["What a tick costs in CPU"](#what-a-tick-costs-in-cpu) | pass |
| resident memory below 50 MiB | median 24.5–26.7 MiB, peak 27.2 MiB | pass |
| no unbounded file-descriptor growth | flat at 3 over a 30-minute soak with the real collector | pass over half an hour; the 12-hour run is still owed |
| idle self CPU median below 1%, p95 below 2% | Overview visible (the row the budget is about): median **0.60–0.85%**, p95 **4.30–9.50%** over three 60 s runs | median met; **p95 fails** |
| the same, with the Battery screen visible | median **1.20–1.70%**, p95 **6.00–8.30%** over three 60 s runs | median clearly worse; p95 ranges overlap — see below |
| no unbounded memory growth over 12 hours | 30 minutes: 30.2 MiB → 28.5 MiB resident, retained history bounded, 0 snapshots dropped | evidence, not the gate — see [`soak-testing.md`](soak-testing.md#runs-on-record) |
| no redraw busy loop | not measured as such | — |

### Why there are two idle rows

§16.1 budgets *idle* self CPU, and idle — the Overview screen, untouched, for the
same 60-second window as every other run here — is the first row. Moving the
sensor group off the medium tier onto its own 30-second cadence (§8.6) was meant to
remove the one call that dominated the old p95: an 85 ms `Components::refresh`
landing in one sample in five. It does what it was built to do on a stopwatch — a read
that now lands in one sample in thirty instead of one in five is real progress — but
the idle median **barely moved**, 0.5–1.1% to 0.60–0.85%, two ranges that overlap; and
the 95th percentile is still over budget: **4.30–9.50%** against 2%. Better than the
pre-release 6–11%, not a pass. Why the median barely moved is now understood, and it is
finding 2 of ["What a tick costs in CPU"](#what-a-tick-costs-in-cpu): the read that was
moved cost wall-clock, not CPU.

**Why is now measured, and the answer is not the one this section originally
guessed.** `scripts/measure-overhead.py` samples about every 0.81 s, not once a
second — 74 samples over a 60-second window — so the 2% p95 budget this
instrument actually checks is closer to "2% of ~810 ms," roughly **16 ms of CPU
per tick**, not 20. A fast-only tick already costs 8.3–17.9 ms of *CPU* on this
workload (["What a tick costs in CPU"](#what-a-tick-costs-in-cpu)): most or all of
that 16 ms is spent before any medium-tier or sensor work is added at all. That is a
real, structural constraint, and it says the lever is *moving* work off the tick the
p95 measures, the way Tasks 1–6 moved the sensor read, rather than trying to make
whatever remains cheaper. There may be very little headroom left in the budget for
that — though this measurement is against ~1000 processes where §16.1's reference
workload is 200, and the per-process walk is the largest part of a fast tick, so how much
of the fast tick's 8.3–17.9 ms survives onto the reference workload is not measured.

What that section adds is which read to move, and it is not the one Tasks 1–6
moved. The medium tier costs **13.2–35.0 ms of CPU** beyond a fast-only tick — from 82%
of the whole 16 ms budget to more than twice it — while the sensor read whose 85 ms motivated
this release turns out to cost **almost no own-process CPU at all**, bounded above at about
4 ms, because that 85 ms is wait rather than computation. The improvement Tasks 1–6 delivered is real and
measured; the reason it did not fix the p95 is that it moved the wrong read for this
particular budget.

The suspect that was named and then retracted was the medium tier's other work,
`Disks::refresh(true)` (the filesystem-capacity read, §8.6), which shares the
fast-plus-medium tick with the now-departed sensor read and was always there. It is
worth recording how that went, because the process was right even where the guess
was.

An earlier draft named it from a wall-clock figure — a fast-plus-medium tick at
36.14 ms median against 9.26 ms fast-only, `Instant::elapsed()` around the whole
tick — and computed `36 ms ÷ 1000 ms ≈ 3.6%` as "the tick's CPU cost". That was
wrong twice over: it treated a wall-clock duration as CPU time, and it divided by
1000 ms rather than the ~810 ms this instrument samples at. It was retracted on the
grounds that this file already knew wall-clock and CPU need not track for a
filesystem-capacity read, so naming the read from a stopwatch would repeat a mistake
the file had caught itself making once before.

Measured on both clocks, the retracted attribution turns out to be **correct**: the
medium tier really does cost 13.2–35.0 ms of CPU, and for *this* read wall-clock did
predict CPU, to within a percent or two. The retraction was still the right call — an
unproven number that happens to be right is not evidence, and the same reasoning
applied to the sensor read (85 ms wall, ~0 ms CPU) would have been badly wrong in the
other direction, as finding 2 above shows. The lesson kept is the one about the
method, not about the disk read.

There is a second loose end, and it does not fit the disk-read story either. A
per-sample diagnostic trace taken during this investigation (45 one-second-ish
samples, Overview idle) showed elevated readings recurring roughly every 3
seconds — not aligned with the 5-second medium tier or the 30-second sensor
cadence. One candidate explanation: macOS's `ps` reports `%cpu` as a
short-time-constant decaying average, so a single CPU burst can smear an
elevated reading across two or three consecutive samples, which could make the
*apparent* ~3 s period an artefact of the sampling cadence beating against the
real one rather than a second genuine periodic cost. **That is a hypothesis, not
a finding** — it has not been measured, and it is recorded here as the caveat it
is rather than folded silently into a tidier story than the data supports.

The honest state, then: the p95 still fails, the improvement from Tasks 1–6 is real
and measured but was aimed at a read that costs wall-clock rather than CPU, and the
medium tier is now a **measured** cause of the remaining miss rather than a suspected
one. Whether it is the whole cause is not settled — see the arithmetic at the end of
["What a tick costs in CPU"](#what-a-tick-costs-in-cpu), which accounts for the lower
part of the observed 4.30–9.50% band but not its top. Bringing the tick under budget
is not part of this round; knowing what to move is now measured rather than guessed.

The second row is the Battery screen, where the sensor group returns to five
seconds because the reader is looking at the thing it measures. Its **median**
is unambiguously worse than the Overview row's — 1.20–1.70% against 0.60–0.85%,
no overlap across three runs each — which is the sensor read's cost showing up
exactly where it should. Its **p95**, 6.00–8.30%, does *not* separate cleanly
from the Overview row's 4.30–9.50%: one Overview run (9.50%) is worse than every
Battery run measured. A p95 of 74 samples is the fourth-largest value in the
window, and which periodic burst a ~0.81 s sampling phase happens to catch during
one 60-second run has more influence on that single number than a three-run
comparison can average out — the median is the number to trust here, and the
p95 ranges overlapping is evidence of how noisy a once-a-run p95 is, not evidence
against the Battery screen costing more. It does not change the gate either way:
§16.1's budget is about *idle*, and idle is the Overview row above — the one this
release was built to bring under budget, and which the measurement above says it
has not yet done.

The workload matters and is not the reference one: §16.1 specifies 8 logical CPUs
and 200 processes, and this machine has 12 CPUs and about a thousand processes — so
the collection figures are against a workload five times larger than the budget
assumes and should be read as a hard case.

The collection row is quoted per *tick shape*, which an earlier measurement got wrong
in a way worth recording. `DueTiers` could only be constructed as `NONE` or `ALL` from
outside its crate, so the measurement used `ALL` for every sample and reported a p95 of
172 ms — the cost of the most expensive tick there is, as though it were the ordinary
one. At the default intervals four ticks in five are fast-only and cost about
9–13 ms (see the row above); `DueTiers::fast_only()` and `fast_and_medium()` now
exist so that the tick a budget is about can actually be measured, and they are
pinned against what `TierScheduler` really produces rather than against their own
definitions.

### What a tick costs in CPU

Every collection figure above this line is wall-clock — `Instant::elapsed()` around
`collector.sample()`. §16.1's idle budget is about **CPU**, and this file has already
caught itself once quoting the one for the other on a filesystem-capacity read (see
"Two things follow", below). So the tick is now measured on both clocks by the same
test, from `crates/monitrs-collectors/src/selfstat.rs`:

* `thread_cpu_time()` — `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, the calling
  thread's own consumed CPU. Narrow on purpose: the difference across one
  `collector.sample()` call on the test's thread can be *charged* to that call.
* `process_cpu_time()` — `ru_utime + ru_stime` from `getrusage(RUSAGE_SELF)`, every
  thread of the process. It exists because the thread clock's narrowness is also a
  blind spot: work a call pushes onto a framework's own thread is real CPU the
  thread clock charges it nothing for. The gap between the two is that work.

```sh
MONITRS_REFERENCE_MACHINE=1 cargo test -p monitrs --release --test capture -- \
  --ignored --nocapture measure_the_sample_collection_budget_per_tick_shape
```

Reference machine, 15 samples per tick shape, 981–1007 processes, **fifteen runs**. Nine
of the fifteen were taken with the instrument as shipped and are the dataset every
wall-clock-dependent figure below is quoted from; the other six measured the thread clock
only, and their thread-CPU medians are given at the end of this section so that the
thread-CPU ranges are checkable too.

The run below is the one whose medium-tier increment is the median of the fifteen, quoted
in full so the three clocks can be compared on one tick:

| Tick shape | Wall-clock median | Thread CPU median | Process CPU median | Process CPU as a share of wall |
|---|---:|---:|---:|---:|
| fast only (4 ticks in 5) | 18.42 ms | 17.89 ms | 17.90 ms | **97%** |
| fast + medium (every 5th) | 41.73 ms | 41.24 ms | 41.25 ms | **99%** |
| every tier (every 30th, sensors included) | 136.03 ms | 29.58 ms | 40.22 ms | **30%** |

> **The CPU column is not additive down the rows, and nobody should sum it.** Every-tier is
> a strict superset of fast+medium — the tier dispatch in `CommonCollector::sample` and
> `MacosCollector::sample` just adds calls, with no cache that a longer tick could satisfy
> instead — yet its measured CPU is *lower* than fast+medium's in this run and in most
> others: across the nine shipped-instrument runs, every-tier's thread CPU is below
> fast+medium's in **9 of 9** (by 3.4–20.2 ms) and its process CPU in **6 of 9** (by up to
> 14.3 ms). **This effect is not explained.** The tick shapes are measured in three
> consecutive blocks, so block-to-block drift on a live host is the obvious candidate, but
> a 9-of-9 sign is more consistent than drift alone would predict, and no mechanism has
> been confirmed. Consequences, taken seriously below: a reader who subtracts these rows to
> price the sensor tier will derive a *negative* cost, and the only safe use of the column
> is a **one-sided bound** (see finding 2), never additive accounting.

**Read the ratios, not the milliseconds.** The absolute figures move a great deal with
what else is on the host — a fast-only tick's CPU median ranged 8.3–17.9 ms across the
fifteen runs, because this is a developer's laptop with a browser open, not a bench rig.
(Where the host's 1-minute load average was recorded during these runs it sat between 4.5
and 8.7; the earliest runs were taken before it was being recorded, so the correlation with
load is an observation about this data set, not a measured relationship.) The CPU-to-wall
shares moved far less, over the nine runs that measured wall-clock alongside CPU:
**88–97% for fast-only, 92–99% for fast+medium, and 27–31% for every tier.** The first two
are quoted from the thread figure and the third from the process figure — for the first two
shapes the choice is immaterial, since the two clocks differ by 5.0–14.0 **µs** there, but
for every-tier it is not: the thread share is 21–25% and the process share 27–31%, and the
process figure is the one that counts a call's work wherever it ran. Quoting the thread
share there would both understate the cost and contradict the 69–73% not-CPU figure below.
The conclusions rest on these ratios and on within-run differences, not on any single
absolute number.

**Three findings, and the second one is not what this release expected.**

**1. The medium tier's cost is real CPU, and it is large.** A fast-plus-medium tick's CPU
is **92–99%** of its wall-clock in all nine runs that measured both, so for *this* read
class the stopwatch was telling the truth. The CPU it spends beyond a fast-only tick ranged
**13.2 to 35.0 ms, median 23.4 ms** over the fifteen runs, and was **positive in 15 of 15**
— from **82% of the whole 16 ms budget at the low end to more than twice it at the high
end**. Fifteen out of fifteen is what makes this a measurement rather than a run of noise,
and it is the difference between this claim and the one Task 7 retracted for the same read
class. Task 7 was right to retract
an attribution it could not support, and the attribution now measured happens to be the one
it retracted — the difference being that this time it is measured on the meter the budget
uses.

The medium tier's work is two filesystem-capacity reads: `Disks::refresh(true)` in
`CommonCollector::refresh_medium` (per-mount `CFURL` capacity, plus building
`cached_filesystems`) and `filesystem::read_inode_usage`'s `getfsstat` in the macOS
layer. **How that CPU splits between the two is not measured here** — this instrument
times a tier, not a call, and the two were never separated. "The medium tier costs tens of
milliseconds of CPU" is the measured claim; any split between its two reads is not, and
separating them is the first thing to do before moving either.

**2. The sensor read costs almost no own-process CPU. Its 85 ms is overwhelmingly wait.**
Two independent pieces of evidence, and the second is the load-bearing one.

*The tick is mostly not running.* On the every-tier tick, **86.3–98.2 ms of the tick is not
CPU on any thread — 68.9–73.2% of it, in all nine of the shipped-instrument runs.** That is
a *within-run* subtraction (this tick's wall minus this tick's own process CPU), so
block-to-block drift cannot manufacture it. It does **not** isolate the sensor read, though:
this tick also carries the slow tier's `Users::refresh`, priced at 30 ms of wall-clock in
the table below, and the arithmetic does not close in the tidy direction anyway — in the run
quoted above, slow+sensors adds 94.3 ms of wall while the two reads inside it are documented
at 30 + 85 = 115 ms. So this figure bounds *the tick*, not either read within it.

*The increment is bounded above, and that is what settles it.* Adding the slow tier and the
sensors to a fast-plus-medium tick changes its process CPU by **−14.3 to +3.6 ms** across
the nine runs. CPU cannot be negative, so whatever the unexplained non-additivity above is
doing, the observed increment is **bounded above by about 4 ms** — and hiding an 85 ms CPU
cost inside it would require an ~80 ms downward artefact, which nothing here suggests and
which drift could not produce nine times running. That bound holds however the tick's
wall-clock divides between the two reads, which is why it survives the objection that this
tick carries `Users::refresh` too: it bounds them *both*. The sensor read is a wait, not
computation.

This corrects the account this file gave of its own release. The 85 ms
`Components::refresh` was quoted as the figure that "dominated the arithmetic"; on a
stopwatch it does, on the meter §16.1 budgets it is close to free. So **Tasks 1–6 moved a
read that was never a significant CPU cost**, which is why the idle median barely moved
(0.5–1.1% → 0.60–0.85%) and the p95 did not come under budget. The work was still worth
doing — a 136 ms tick occupies the sampler thread whether or not it burns CPU, and §16.1
budgets collection wall-clock too — but it was not the p95 lever, and this file said it
was.

*Where the time goes instead is not measured.* `sysinfo` 0.39.6's arm64 component path
reaches the sensors through `IOHIDServiceClientCopyEvent`, which is in-kernel
IOHIDFamily/SMC rather than a userland daemon, so the candidates are time attributed to
`kernel_task` and plain hardware latency waiting on the SMC — not a separate user process
this program could be said to be driving. Either way it is outside §16.1's own-CPU budget
and still real for the user. **Hypothesis, not a finding**: nothing here measured where the
waiting time is spent.

**3. A thread clock alone would have understated finding 2.** On the every-tier tick the
process figure exceeds the thread figure by **5.9–10.6 ms** in all nine runs; on the other
two shapes the two agree to within **5.0–14.0 µs**. Something in the sensor path does
several milliseconds of real work on a thread that is not the caller's, so the thread clock
— whose narrowness is what makes findings 1 and 2 attributable — is also blind to it, and
both readings are needed.

That microsecond-level agreement is itself the calibration that matters most for the other
two findings. A per-thread CPU clock's most likely silent failure on Darwin is dropping
kernel time; on a tick that makes on the order of a thousand `proc_pidinfo` calls, a clock
that missed system time could not land within 14 µs of one that counts it. So the thread
figure is not merely narrow, it is **complete** — which is what licenses charging the
medium tier's 13.2–35.0 ms to the medium tier.

An independent process-CPU source corroborates this: `pti_total_user + pti_total_system`
from `PROC_PIDTASKINFO`, converted through the mach timebase, taken in three earlier runs
in place of `getrusage`. It agreed with the thread clock to within 0.03 ms on the fast and
fast-plus-medium shapes — just as `getrusage` does — and showed the same every-tier gap,
7.3–8.3 ms. The two process sources were never sampled in the *same* run, so this is two
methods independently reproducing the effect rather than a direct comparison between them.
(That reading also has a trap worth recording: `proc_pidinfo`'s CPU totals are in mach
absolute time units, not nanoseconds, so a first attempt read 41× too small — the same
timebase correction `macos/process.rs` already documents.)

**The other six runs**, so that the thread-CPU ranges above can be checked rather than
taken on trust. Thread-CPU medians only — these runs did not record wall-clock beside CPU,
which is why every share and every not-CPU figure above is scoped to the nine that did.
Fast-only / fast+medium / every-tier, in ms: `9.12 / 28.65 / 25.81`, `8.41 / 22.37 / 26.12`,
`8.33 / 21.51 / 26.23`, `9.56 / 30.09 / 26.85`, `9.31 / 28.35 / 26.61`, `11.47 / 29.16 /
26.86`. The last three are the `PROC_PIDTASKINFO` runs.

One caveat about the shape of every figure here: with 15 samples per tick shape, the
"p95" this test prints is `samples[15 * 95 / 100]` = `samples[14]`, i.e. **the maximum**.
That is pre-existing in the wall-clock path and the CPU columns follow it for consistency,
but a "CPU p95" in this test's output is a worst-of-15, not a percentile, and the medians
are the figures to reason from.

#### What this says about the failing p95

Task 7's arithmetic puts the whole tick at roughly **16 ms of CPU** for a 2% p95 on
`scripts/measure-overhead.py`'s ~0.81 s sampling interval. Against that:

* a fast-only tick costs 8.3–17.9 ms of CPU — **1.0–2.2%** of the interval on its own,
  i.e. most or all of the budget before any medium-tier work exists;
* the medium tier's *increment alone*, 13.2–35.0 ms, is 82% of the entire budget at its
  low end and more than twice it at its high end;
* a fast-plus-medium tick costs 21.5–50.5 ms of CPU, which at ~810 ms is **2.7–6.2%** —
  against an observed idle p95 of 4.30–9.50%.

The predicted 2.7–6.2% and the observed 4.30–9.50% overlap across most of both bands,
and a fast-plus-medium tick is comfortably enough on its own to fail a 2% budget. It is
still not established that it accounts for *all* of the observed range: the top of that
band (9.50%) is above what these figures predict, `ps` reports `%cpu` as a decaying
average which smears a burst across neighbouring samples, and the ~3 s periodicity noted
in ["Why there are two idle rows"](#why-there-are-two-idle-rows) is still unexplained.
**The medium tier is now a measured cause of the p95 miss; whether it is the only one is
not settled.**

What follows for the release is that the lever is the medium tier's
filesystem-capacity work, on the same "move it, do not micro-optimise it" reasoning
Tasks 1–6 used — and with the caveat those tasks earned: check the CPU cost of a read
before moving it for CPU reasons, because on this platform a read's wall-clock time
predicted its CPU time well for the disk reads and not at all for the sensors.

### Where the idle CPU goes

Not into monitrs' own computation. A fast tick's pure computation is about 35 µs
(see the microbenchmarks below); the OS reads are three orders of magnitude more
expensive. Measured individually against 981 processes, 2 disks, 25 thermal
components and 21 interfaces:

| Read | Cost | Tier |
|---|---|---|
| `sysinfo` process refresh | 29 ms | ~~fast~~ — **fixed**, see below |
| `Disks::refresh(false)` | 34 ms wall, ~21 ms of it our own CPU | ~~fast~~ — **fixed**, see below |
| `Disks::refresh_specifics(io_usage)` | ~1 ms CPU | fast |
| `Disks::refresh(true)` | 25 ms wall-clock for the call; the medium **tier** it sits in is 92–99% CPU, so this is CPU rather than blocked wait — but the per-call CPU split against the tier's `getfsstat` is not measured (see ["What a tick costs in CPU"](#what-a-tick-costs-in-cpu)) | medium |
| `Components::refresh` (temperatures) | 85 ms wall-clock, and **almost none of it own-process CPU**: adding this read and the slow tier together changes a tick's process CPU by −14.3 to +3.6 ms, so the two are bounded above by about 4 ms of CPU between them (same section) | ~~medium~~ — **sensors**, its own 30 s / 5 s cadence, see below |
| `Users::refresh` | 30 ms | slow |
| `Networks::refresh` | 0.85 ms | fast |
| global CPU + memory | 0.09 ms | fast |

A fast tick started at about 64 ms. With the two fixes above the assembled collector
measures **8–16 ms** for one — the native walk, the per-process counters, and the disk
I/O counters — and the measured idle median fell from 3.7–5.1% to **0.5–1.1%**, meeting
the 1% budget.

That was the whole story before this release. §8.6 put `Components::refresh` on the
same 5-second medium tier as `Disks::refresh(true)`, and the 85 ms sensor read — six
times the disk read's own 25 ms — dominated the arithmetic: one spike in the second
it landed, one sample in five, and a p95 taken once a second saw it every time.
Moving the sensor group to its own 30-second cadence (5 seconds only while the
Battery screen is visible, §8.6) was the fix this release made, and it worked as
designed: the sensor spike now lands in one sample in thirty rather than one in
five. The idle median barely moved, though — 0.5–1.1% to 0.60–0.85%, overlapping ranges —
and ["What a tick costs in CPU"](#what-a-tick-costs-in-cpu) explains why: the 85 ms below
is wall-clock, and the read costs almost no CPU.

**The p95 still fails: 4.30–9.50% against a 2% budget, and the remaining cause is
the medium tier — measured, not suspected.** That measurement, the instrument behind
it and its limits are in ["What a tick costs in CPU"](#what-a-tick-costs-in-cpu) and
are not repeated here; the short form is that the tier's CPU increment over a
fast-only tick is 13.2–35.0 ms against a whole-tick budget of roughly 16 ms, and that
which of the tier's *two* filesystem-capacity reads carries it is the part still open,
because they were never measured apart. Moving the sensor read fixed the read this
release targeted; it did not, on its own, bring the idle p95 under §16.1's budget.

Two things follow, and neither is a micro-optimisation:

1. **Asking `sysinfo` for fewer process fields does not help — but not asking at all
   does.** *Fixed.* Requesting `ProcessRefreshKind::nothing()` still costs 26 ms of the
   29, because the per-process `proc_pidinfo` calls that validate each entry are the
   cost rather than the fields. In CPU terms the walk is **30.8 ms**, against 2.3 ms for
   the native `kern.proc.all` walk and 6.0 ms for the per-process counters that the
   macOS layer already makes — and that layer replaces every row the baseline produces.
   So the baseline is now told to stop producing them
   (`CommonCollector::delegate_process_table`), and the four fields the native row still
   took from it — identity, name, command, executable — come from the native side.

   The interesting part is the **name**, because that is where this could have quietly
   become a regression. Three candidates were measured against 1030 live processes:
   `p_comm` matched 591 and is a *prefix* of the rest, since `MAXCOMLEN` truncates it to
   16 bytes; the executable's file name matched 1022; `argv[0]` accounts for the last 8.
   `argv[0]` was tried first and the test comparing the two collectors is what rejected
   it — a process may write anything there, and the measured contents included
   `Cursor Helper (Plugin): extension-host (agent-exec) servicrab [2-14]`,
   `server-memory@2026.1.26` for a `node` process, and `-zsh` for a login shell. The
   rule is therefore the executable's file name, preferring the path the process was
   *launched* from (free, in the argument blob) over `proc_pidpath`'s resolved one, so a
   versioned install shows `claude` rather than `2.1.220`. It differs from `sysinfo` for
   a handful of processes, in the bounded direction.

   Immutable per process, so it is read once and cached by identity — measured at 1.7 ms
   of CPU for a thousand processes, i.e. once, not per tick.
2. **`Disks::refresh(false)` was the single most expensive line in the fast tier, and
   for something the fast tier never reads.** *Fixed.* The call reads as "refresh,
   cheaply or not"; it is not. It calls
   `refresh_specifics(bool, DiskRefreshKind::everything())`, and the `bool` is only
   `remove_not_listed_disks`. `everything()` includes `storage`, which on macOS is a
   per-mount volume-capacity query through `CFURL`'s resource properties — about 21 ms
   of our own CPU per tick against two volumes, once a second, for a number that is a
   *medium*-tier metric (§8.6) published from `cached_filesystems`. The fast tier reads
   `usage()` and `name()` and nothing else, so it now asks for
   `DiskRefreshKind::nothing().with_io_usage()`: ~1 ms. Measured end to end, the idle
   median went from 3.7–5.1% to 1.3–2.7%.

   Worth noting *why* the earlier measurement missed this: it timed wall clock, and a
   `CFURL` capacity query blocks without burning much CPU, so the two look similar on a
   stopwatch and nothing like each other on the meter §16.1 actually budgets. The
   collection figures above are wall-clock and barely moved; the idle-CPU figure
   halved.

The honest statement is the one in the table, row by row: four budgets pass outright, the
descriptor budget passes over half an hour rather than the twelve the gate asks for, the
idle-CPU **median** now passes and its **p95** does not — 4.30–9.50% against 2%. The 85 ms
sensor read this release moved off the shared schedule is ruled out as the cause, measured
rather than assumed — and now understood: it was never a CPU cost at all, only a
wall-clock one (["What a tick costs in CPU"](#what-a-tick-costs-in-cpu)). What is
responsible is the medium tier's filesystem-capacity work, at 13.2–35.0 ms of CPU per
medium tick against a ~16 ms budget for the whole tick, measured on a thread and a
process CPU clock rather than inferred from a stopwatch. Whether it is the *whole*
cause is still open, and two rows have no measurement to pass or fail.

### What a sensor read costs

Measured by `crates/monitrs-collectors/benches/sensor_cost.rs`
(`cargo bench -p monitrs-collectors --bench sensor_cost`) on the reference machine, 25
components discovered — the same count the live measurement above saw:

| Variant | Time |
|---|---|
| `Components::refresh(true)` — what the collector did before 1.0.0 | 80.1 ms |
| `Components::refresh(false)` — no list rebuild | 85.6 ms |
| one component refreshed alone | 334 µs |

The list rebuild is not where the 85 ms goes: `refresh(false)` costs no less than
`refresh(true)`, if anything a touch more. Reading `sysinfo` 0.39.6's arm64 path
(`src/unix/apple/macos/component/arm.rs`) says why — `ComponentsInner::refresh`
re-enumerates every matching IOKit HID service and re-reads each one's name and serial
`CFString`s on every call regardless of the bool; that enumeration is the cost, not the
`retain_mut` the bool actually controls. `Component::refresh`, by contrast, already
holds the service handle and goes straight to `IOHIDServiceClientCopyEvent`: 334 µs,
about 240× less than reading all 25 through the list.

**A cheaper read exists.** The 85 ms buys re-discovering and re-identifying every
sensor on the machine, not reading a temperature: a component that already knows which
service it is can be refreshed for a few hundred microseconds instead of tens of
milliseconds. A scheduler that keeps a handle to the one component the header shows,
rather than calling `Components::refresh` for the whole list on every sensor-tier tick
— every 30 seconds idle, every 5 seconds while the Battery screen is visible (§8.6) —
has that spike to gain back on whichever cadence it lands on.

### Reading the idle-CPU budget on its own reference workload

Every idle-CPU figure above — the passing median and the failing p95 alike — was
measured on this repository's own development machine: 12 logical cores and
roughly a thousand processes, not §16.1's stated 8 logical CPUs and 200 processes.
That is not a rounding difference: the dominant costs are per-process and
per-device OS reads, so a host running five times the reference process count
costs several times what the budget was written against. The budget has never
actually been read on the workload it names. This is that protocol, for whoever
next has an 8-vCPU machine to hand — most likely the EC2 instance Task 12 already
runs the tagged release archives on for the platform smoke tests.

**Instance shape.** Any instance advertising 8 vCPUs (an `m5.2xlarge` or
equivalent) approximates the reference CPU count. Its process count out of the
box is not measured here — this file has already been misled once by assuming a
process count instead of printing one — so no figure for it is guessed at either;
`scripts/measure-overhead.py` now prints the host's actual count beside its
verdict, and that printed count, not an expectation set out in this paragraph, is
what decides whether a synthetic process farm is needed to approach 200.

**Binary.** Do not build on the instance. Task 12 is already extracting the
tagged release archive there to run the smoke tests, so reuse that extraction
instead of a second one:

```sh
tar xzf monitrs-X.Y.Z-<target>.tar.gz
cd monitrs-X.Y.Z-<target>
```

**Commands**, run from a checkout of this repository at the tagged commit,
pointed at the extracted binary rather than a locally built one — the script
needs Python 3 and nothing else:

```sh
python3 -m py_compile scripts/measure-overhead.py
MONITRS_BINARY=/path/to/monitrs-X.Y.Z-<target>/monitrs python3 scripts/measure-overhead.py
```

The printed `monitrs: 1s sampling interval, 300s (5 min) history` line is not
something to configure: those are `--no-config`'s compiled defaults
(`crates/monitrs/src/config.rs`), and they already are §16.1's reference interval
and history span. The `workload:` line above it, and the sentence under it
saying whether the workload matches the reference, are what to check before
trusting anything below them — see `scripts/measure-overhead.py`'s own docstring
for why a Linux run's CPU figure is sourced differently from a Darwin one and
must not be read as the same kind of number for any *other* reason.

**Where the numbers go.** A new row beside the Mac's, in
["The §16.1 end-to-end budgets"](#the-161-end-to-end-budgets): the reference host
as the gate, the ~1000-process Mac as the hard case, which is the framing this
file already gives every other measurement pair here. Name the instance type,
vCPU count, OS and the printed process count, the same way the reference-machine
table at the top of this file names this one.

**What each outcome means, decided in advance:**

* **p95 under 2% on the reference workload:** the budget is met on the workload
  it names. Both figures get published side by side — the reference host as the
  gate that passes, the ~1000-process Mac as the hard case that does not — and
  `docs/release-checklist.md`'s idle-CPU box can be ticked for the reference
  workload, with the Mac's numbers carried alongside as the harder case rather
  than dropped.
* **p95 still over 2% on the reference workload too:** the miss is not an
  artefact of measuring a workload five times the size the budget names, and the
  medium tier's measured 13.2–35.0 ms of CPU
  (["What a tick costs in CPU"](#what-a-tick-costs-in-cpu)) is the next thing to
  move — moved, not micro-optimised, the same lever Tasks 1–6 used on the sensor
  read. That changes the release decision: the idle-CPU budget would be a real
  miss on its own named workload, not only on a harder one nobody asked for.

Neither outcome is written here in advance. Task 12 runs the commands; this file
gets whichever numbers come back.

## What is not measured yet

Named here so their absence is not mistaken for a passing grade. §16.3's own list of
ten benchmarks is now complete; what is left is end-to-end.

* The 12-hour soak test. A 30-minute run with the real collector is on record in
  [`soak-testing.md`](soak-testing.md#runs-on-record) and shows no growth, but the
  §16.1 gate is twelve hours and that has not been run. Nor has any soak on Linux.
* Whether a slow terminal can block a frame. `capture.rs` renders through ratatui's
  `TestBackend`, which stops short of the write to the terminal, and the soak has no
  renderer at all — so a pty consumer that stops reading is invisible to both.
  (The 90 ms worst-case input latency an earlier soak reported is *resolved*, not
  outstanding: it was the measurement, not the program — see
  [`soak-testing.md`](soak-testing.md).)
* The end-to-end numbers above on the §16.1 reference workload — 8 CPUs and 200
  processes — rather than on this machine. `scripts/measure-overhead.py` can
  now take that reading directly; see
  ["Reading the idle-CPU budget on its own reference workload"](#reading-the-idle-cpu-budget-on-its-own-reference-workload)
  for the protocol. Running it is Task 12's, and it has not been run yet.
* Whether anything constitutes a redraw busy loop, as opposed to the idle-redraw
  interval the reducer enforces.
* **How the medium tier's CPU splits between its two reads.** The tier costs
  13.2–35.0 ms of CPU (["What a tick costs in CPU"](#what-a-tick-costs-in-cpu)), but
  that instrument times a *tier*, not a call, and the tier holds two
  filesystem-capacity reads: `Disks::refresh(true)` and `read_inode_usage`'s
  `getfsstat`. Which of the two dominates is not measured, and a fix aimed at the
  wrong one would buy nothing.
* **Where the sensor read's 85 ms actually goes.** Its own-process CPU is bounded above at
  about 4 ms, and the tick carrying it spends 86.3–98.2 ms not running on any of our threads,
  so the time is going somewhere outside this process. `IOHIDServiceClientCopyEvent` is
  in-kernel IOHIDFamily/SMC, so the candidates are time booked to `kernel_task` and plain
  SMC latency rather than a userland daemon — but nothing here measured either, so it stays a
  hypothesis. It matters because time the kernel spends on our behalf is invisible to §16.1's
  own-CPU budget and still real for the user.
* **Whether the medium tier accounts for the whole idle p95.** It is enough on its own
  to fail the 2% budget, but the top of the observed 4.30–9.50% band is above what the
  measured per-tick CPU predicts, and the ~3 s periodicity noted in "Why there are two
  idle rows" is still unexplained.

## Reading these numbers

Sum the relevant rows for a per-tick estimate, then remember what is missing.
For the 200-process reference workload, one fast tick's *pure computation* is
roughly: rate updates (1.7 µs) + history record (14 µs) + sort (17 µs) + filter
(1.5 µs) ≈ 35 µs, plus formatting for the visible rows only. Against a 200 ms p95
collection budget, computation is not the constraint — the OS reads are. That is
the useful conclusion, and it is the reason no micro-optimisation has been applied
to any of the above.
