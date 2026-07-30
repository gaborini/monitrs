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
| sample collection below 200 ms p95 | fast-only tick (4 in 5): median 8–16 ms, p95 15–21 ms. Fast+medium (every 5th): median 110–121 ms, p95 121–161 ms | pass |
| resident memory below 50 MiB | median 24.5–26.7 MiB, peak 27.2 MiB | pass |
| no unbounded file-descriptor growth | flat at 3 over a 30-minute soak with the real collector | pass over half an hour; the 12-hour run is still owed |
| idle self CPU median below 1%, p95 below 2% | median **0.5–1.1%** over three runs, p95 **6–11%** | median met; **p95 fails** |
| no unbounded memory growth over 12 hours | 30 minutes: 30.2 MiB → 28.5 MiB resident, retained history bounded, 0 snapshots dropped | evidence, not the gate — see [`soak-testing.md`](soak-testing.md#runs-on-record) |
| no redraw busy loop | not measured as such | — |

The workload matters and is not the reference one: §16.1 specifies 8 logical CPUs
and 200 processes, and this machine has 12 CPUs and about a thousand processes — so
the collection figures are against a workload five times larger than the budget
assumes and should be read as a hard case.

The collection row is quoted per *tick shape*, which an earlier measurement got wrong
in a way worth recording. `DueTiers` could only be constructed as `NONE` or `ALL` from
outside its crate, so the measurement used `ALL` for every sample and reported a p95 of
172 ms — the cost of the most expensive tick there is, as though it were the ordinary
one. At the default intervals four ticks in five are fast-only and cost 36–50 ms;
`DueTiers::fast_only()` and `fast_and_medium()` now exist so that the tick a budget is
about can actually be measured, and they are pinned against what `TierScheduler`
really produces rather than against their own definitions.

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
| `Disks::refresh(true)` | 25 ms | medium |
| `Components::refresh` (temperatures) | 85 ms | medium |
| `Users::refresh` | 30 ms | slow |
| `Networks::refresh` | 0.85 ms | fast |
| global CPU + memory | 0.09 ms | fast |

A fast tick started at about 64 ms. With the two fixes above the assembled collector
measures **8–16 ms** for one — the native walk, the per-process counters, and the disk
I/O counters — and the measured idle median fell from 3.7–5.1% to **0.5–1.1%**, meeting
the 1% budget.

What remains is the p95, at 6–11% against a 2% budget, and it is the medium tier: the
85 ms `Components::refresh` for temperatures, which §8.6 puts on a 5-second schedule.
Averaged that is 1.7% of a core, but it arrives as one spike in the second it lands, and
a p95 taken once a second sees the spike. Nothing has been measured about whether that
read can be made cheaper, so it is the next thing to look at rather than a conclusion.

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
idle-CPU **median** now passes and its **p95** does not — 6–11% against 2%, arriving as the
medium tier's 85 ms `Components::refresh` once every five seconds — and two rows have no
measurement to pass or fail. The reason for the p95 is measured rather than guessed, and
whether that read can be made cheaper is the open question, not which read it is.

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
  processes — rather than on this machine.
* Whether anything constitutes a redraw busy loop, as opposed to the idle-redraw
  interval the reducer enforces.

## Reading these numbers

Sum the relevant rows for a per-tick estimate, then remember what is missing.
For the 200-process reference workload, one fast tick's *pure computation* is
roughly: rate updates (1.7 µs) + history record (14 µs) + sort (17 µs) + filter
(1.5 µs) ≈ 35 µs, plus formatting for the visible rows only. Against a 200 ms p95
collection budget, computation is not the constraint — the OS reads are. That is
the useful conclusion, and it is the reason no micro-optimisation has been applied
to any of the above.
