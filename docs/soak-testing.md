# Soak testing

§16.1 sets two budgets that a unit test cannot check:

* **no unbounded memory growth over a twelve-hour run;**
* **no unbounded file-descriptor growth.**

§16.2 adds three more, under load: input stays responsive, snapshots coalesce, and
queues never grow without bound.

The harness for all five is `crates/monitrs/tests/soak.rs`. It drives the real
worker threads, the real bounded channel, the real reducer, and the real history
ring, and it takes its run length from the environment: seconds by default, hours
when you ask. This file is the operator's half — how to run it, how to read it, and
what to write down.

## The short version

```sh
cargo test -p monitrs --test soak -- --ignored --nocapture
```

Ten seconds, a 4,000-process fake system sampled fifty times a second, and a
report on stdout. `--ignored` because the test is marked `#[ignore]`; `--nocapture`
because the report is printed rather than logged.

CI runs exactly this on the two runners that execute tests — Linux x86_64 and macOS
arm64 — as part of its ignored-test pass, so the harness itself cannot rot unnoticed.
The other four release targets are build-only, so they are never soaked by CI. And ten
seconds is not a release gate in any case: it proves the plumbing, not the twelve
hours.

## The release soak

Before tagging a release, run all three of these and keep the reports. Each takes
the time it says it takes.

```sh
# 1. Twelve hours, release profile, the shipped history configuration.
MONITRS_SOAK_SECONDS=43200 \
  cargo test --release -p monitrs --test soak -- --ignored --nocapture \
  | tee soak-12h.txt

# 2. High load: §16.2's ten thousand processes, one hour.
MONITRS_SOAK_SECONDS=3600 MONITRS_SOAK_PROCESSES=10000 \
  cargo test --release -p monitrs --test soak -- --ignored --nocapture \
  | tee soak-10k.txt

# 3. The real collector, which is the only mode that opens files at all.
MONITRS_SOAK_REAL_COLLECTOR=1 MONITRS_SOAK_SECONDS=3600 MONITRS_SOAK_INTERVAL_MS=1000 \
  cargo test --release -p monitrs --test soak -- --ignored --nocapture \
  | tee soak-real.txt
```

Run them on a machine you are not otherwise using, and prefer a wall socket to a
battery: a laptop that suspends mid-run produces a report about suspension.

At or above five minutes the harness switches from the smallest supported history
ring to the **shipped** configuration — 300 samples over five minutes — so a long
run soaks what users actually get. Below five minutes it uses the smallest ring,
because a 300-sample ring cannot both fill and then demonstrate a flat trend inside
a ten-second run.

## Knobs

| Variable | Default | What it does |
|---|---|---|
| `MONITRS_SOAK_SECONDS` | `10` | Run length. `43200` is §16.1's twelve hours. |
| `MONITRS_SOAK_INTERVAL_MS` | `20` | Fast-tier sample interval. Lower is more load. |
| `MONITRS_SOAK_PROCESSES` | `4000` | Processes in the fake system. Ignored by the real collector. |
| `MONITRS_SOAK_REAL_COLLECTOR` | unset | `1` drives `CommonCollector` instead of `FakeCollector`. |

An unparsable value falls back to the default rather than failing, because a typo
in a twelve-hour invocation should not be discovered twelve hours later. The
resolved configuration is printed at the top of every report — check it against
what you meant.

Derived, not configurable:

* measurements are taken `run / 24` apart, clamped to 100 ms–60 s, so a twelve-hour
  run produces 720 of them and a ten-second run 18;
* the measurement phase begins only once the history ring is **full**, because
  retained history legitimately grows until then and would look like a leak;
* a run may overshoot its requested length to finish collecting at least eight
  measurements. A slow machine and a broken build must not produce the same result.

## Reading the report

```text
--- monitrs soak report ---
run:            20s requested, 21.5s elapsed (2.8s warm-up)
collector:      fake (4000 processes requested)
load:           20ms fast tier, history 120 of 120 samples, ceiling 2205120 B
snapshots:      882 processed, last sequence 883, 55 detail replies
channel:        peak depth 2 of 64, 12 coalesced, 0 dropped
stall probe:    depth 64 of 64 after the stall, coalesced 2 -> 12
input:          352 keys, median 33.167µs, worst 5.839959ms, 0 unanswered
resident:       first quartile 23017 KiB, last quartile 29760 KiB, peak 29760 KiB
descriptors:    first quartile peak 3, last quartile peak 3
workers:        4 spawned, 0 failed to join
measurements (21):
    8.916µs  rss    22944 KiB  fds    3  snapshots      120  history  621675 B
  ...
```

| Line | What to look for |
|---|---|
| `run` | Elapsed should be close to requested. A large overshoot means the ring took a long time to fill. |
| `snapshots` | The sequence number should be at or above the processed count; the difference is coalescing. |
| `channel` | Peak depth must never exceed the bound (64). `dropped` must be `0`: a coalesced snapshot is a *counted* loss, a dropped keypress is a bug. |
| `stall probe` | The channel is deliberately left undrained. Depth must stop at the bound and the coalesced counter must move. |
| `input` | Median latency is the §16.2 responsiveness figure. `unanswered` above 1 means the UI stalled. |
| `resident` | Compare the quartiles. Expect the first to be slightly lower — see below. |
| `descriptors` | Expect exactly flat. |
| `measurements` | The series. `snapshots` must rise at every point; `history` must stop rising once the ring is full. |

### Why the first quartile is lower than the last

Neither glibc's nor macOS's allocator returns freed pages promptly, so resident
size climbs for a few seconds after the load starts and is then flat. Measured on
the reference machine below, a release build settled after about twelve seconds and
then held to the kilobyte for the rest of the run. The test tolerates the larger of
25% or 16 MiB between the quartiles for that reason.

That tolerance is not as loose as it looks. One retained snapshot at 4,000
processes is on the order of a megabyte, and the sampler produces fifty a second:
leaking anything the sampling loop touches crosses 16 MiB in well under a second.
On a twelve-hour run the settling period is 0.03% of the series and the quartiles
are hours apart, which is exactly why the twelve-hour run — not the ten-second one
— is the release gate.

## What to record

For each of the three release runs, keep in the release record:

1. the **whole report**, verbatim, including the measurement series;
2. the machine: CPU, core count, memory, OS version, architecture;
3. the toolchain (`rustc --version`) and the profile (`--release` or not);
4. what else the machine was doing;
5. an independent resident-size reading taken from outside the process, so the
   figure does not rest solely on our own measurement:

   ```sh
   # Linux, while the soak runs, in another shell:
   watch -n60 'grep VmRSS /proc/$(pgrep -f soak- | head -1)/status'

   # macOS, same idea:
   while sleep 60; do ps -o rss=,vsz= -p "$(pgrep -f 'soak-' | head -1)"; done
   ```

   `ps` reports kibibytes. It will not agree exactly with the report's figure —
   they are sampled at different instants — but it must agree on the *trend*.
6. any assertion that failed, with the series that produced it.

A run that failed and was understood is worth more in the record than a run that
passed and was not read.

## What this does not cover

Be exact about this; §23 asks for claims to be supported by a test or a documented
measurement, and these are neither yet.

* **The interactive runtime is not soaked.** The renderer, the terminal guard, and
  the input thread are absent — the input thread because `crossterm::event::poll`
  needs a real tty, and the renderer because there is no assembled event loop to
  drive yet. So §16.1's *idle CPU*, *frame time*, and *no redraw busy loop* budgets
  are **not** measured here. They need a `monitrs` that launches.
* **Descriptors are flat on macOS by construction**, in both modes. The macOS
  collector reads through `sysctl`, `libproc`, mach routines and `getifaddrs`, and
  opens no file at all (see [`platform-support.md`](platform-support.md)). A flat
  macOS descriptor curve therefore confirms the runtime holds nothing open; it says
  nothing about the Linux collector, which reads `/proc` files constantly. **Run the
  real-collector soak on Linux** before believing the descriptor budget.
* **Absolute resident size is not §16.1's 50 MiB figure.** The harness reports its
  own whole-process footprint, which includes the test binary and a fake system far
  larger than the reference workload. The 50 MiB budget applies to the shipped
  binary at 200 processes and is checked with the interactive runtime, not here.
* **Progressive reduction of expensive enrichment under load** (§16.2) is not
  exercised: the fake collector has no expensive enrichment to shed.
* **Per-function costs** live in [`benchmarks.md`](benchmarks.md). This file is
  about the pipeline over time; that one is about individual operations.

## When a run fails

The report is printed before the assertions, so a failure always comes with its
series. Read the series first.

| Symptom | Where to look |
|---|---|
| `resident memory trended upward` | Is the rise monotone, or a step that plateaus? A plateau is the allocator; a straight line is a leak. Check what the reducer retains per snapshot. |
| `file descriptors trended upward` | Something in the sampling path is not closing. On Linux, `ls -l /proc/<pid>/fd` during the run names it. |
| `the channel grew past its bound` | Someone replaced a bounded channel with an unbounded one, or the coalescing policy stopped applying. |
| `a keypress must never be silently lost` | `EventSender::send`'s non-coalescable path timed out, which means the UI thread was blocked for over 100 ms. |
| `median input latency exceeds` | The reducer got slower. `cargo bench -p monitrs --bench pipeline` will say which part. |
| `the history ring did not fill` | The run was too short for the machine. Raise `MONITRS_SOAK_SECONDS` — the failure message says so too. |
| `only N measurements` | Same cause, same fix. |
| `self-measurement is not implemented for this build` | Not a failure, but the memory and descriptor budgets were **not** verified. See below. |

## Platforms where the footprint cannot be measured

Our own resident size and descriptor count come from
`monitrs_collectors::selfstat`, which reads `/proc/self` on Linux and `libproc` on
macOS. On any other platform — or on macOS built without the `macos-native`
feature — both are `Unsupported`, and the harness says so and *skips* the two
budgets rather than reporting a flat curve it never measured. The rest of the run
still asserts everything else.

## Reference measurements

Not a claim about the release; a record of what the harness produced on one machine
so a future run has something to compare against.

| | |
|---|---|
| CPU | Apple M4 Pro, 12 logical / 12 physical cores |
| Memory | 48 GiB |
| OS | macOS 26.5.2 (arm64) |
| Toolchain | rustc 1.97.1 |

| Run | Snapshots | Peak queue | Coalesced | Dropped | Median input | Resident (first → last quartile) | Descriptors |
|---|---|---|---|---|---|---|---|
| 10 s, debug, 4,000 procs, 20 ms | 435 | 2 of 64 | 11 | 0 | 47 µs | 19,968 → 22,224 KiB | 3 → 3 |
| 45 s, debug, 4,000 procs, 20 ms | 1,969 | 3 of 64 | 15 | 0 | 46 µs | 23,078 → 25,488 KiB | 3 → 3 |
| 20 s, release, 4,000 procs, 20 ms | 882 | 2 of 64 | 12 | 0 | 33 µs | 23,017 → 29,760 KiB | 3 → 3 |
| 40 s, debug, 10,000 procs, 20 ms | 1,539 | 5 of 64 | 216 | 0 | 16.7 ms | 284,857 → 278,044 KiB | 3 → 3 |
| 25 s, debug, real collector, 1,000 ms | 253 | 2 of 64 | 9 | 0 | 34 µs | 22,826 → 22,864 KiB | 3 → 3 |

In the 45-second debug run, resident size reached 25,488 KiB after about ten
seconds and then did not change by a single kibibyte for the remaining thirty —
which is the shape a passing soak has.

The 10,000-process row is the §16.2 one, and it is the only configuration above
where the UI genuinely falls behind: 205 of the 216 coalesced snapshots happened
during the run rather than during the stall probe, so the reducer was shedding load
throughout — and the worst single input latency was still 47 ms. Its resident figure
is the size of a synthetic 10,000-process system in a debug build, not a monitrs
footprint; the trend, which is what the budget is about, was flat to slightly
downward.

No twelve-hour run has been recorded yet. §23's release list is not satisfied until
one has been, and the release checklist
([`release-checklist.md`](release-checklist.md)) treats it as a blocking step.
