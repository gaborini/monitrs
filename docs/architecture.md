# Architecture

This document explains how monitrs is put together and, where a choice was not
obvious, why. For the decisions themselves — including the rejected
alternatives — see [`adr/README.md`](adr/README.md).

## Crate boundaries

```
monitrs-core          data model, rate engine, history ring, process list logic,
                      diagnostics. No terminal library. No OS collector.
       ^         ^
       |         |
monitrs-collectors   monitrs-tui
sysinfo baseline +   ratatui rendering, reducer, keymap, layout, themes
native Linux/macOS
       ^         ^
       |         |
       monitrs (binary)
       CLI, config, runtime wiring, export
```

The direction is enforced by Cargo, not by convention. `monitrs-collectors` and
`monitrs-tui` do not depend on each other and cannot be made to.

Two consequences carry most of the value:

* **The TUI cannot know how `/proc` is parsed.** It receives a `SystemSnapshot`
  and renders it. That is why rendering is testable against a fixture with no
  live system.
* **Collectors cannot know how a process row is coloured.** They produce data and
  availability, never presentation. A collector has no way to express "show this
  in amber".

`monitrs-core` additionally has no way to touch a real system at all, so rate
arithmetic, history seeking, tree construction, and diagnostic rules are all
exercised deterministically.

## Snapshot flow

```
  terminal input thread ──┐
  (only thread that may   │
   call crossterm poll)   │
                          ├──> bounded channel ──> reducer ──> app state ──> render
  sampler thread ─────────┤        (coalescing)       │
  (publishes Arc<Snapshot>)│                          └──> Effect ──> runtime performs it
                          │
  detail worker ──────────┘
  (on-demand, per selection)
```

A collector builds a **complete new snapshot** and publishes it as
`Arc<SystemSnapshot>`. Nothing is updated in place, so the UI can never observe
CPU from one tick next to memory from another. Cloning the `Arc` is how the UI
holds a snapshot across frames without copying it.

Every snapshot carries three time values, and the distinction matters:

| Field | Type | Used for |
|---|---|---|
| `captured_at` | `Instant` | rate arithmetic and ordering |
| `wall_time` | `SystemTime` | display and export only |
| `elapsed` | `Duration` | the *actual* interval since the previous snapshot |

Rates divide by `elapsed`, never by an assumed one second. Suspend/resume, system
load, and scheduler delay all make the real interval variable, and a laptop lid
closed for an hour would otherwise produce a rate three thousand times too large.
Ordering uses `Instant` so that a wall-clock adjustment — NTP step, timezone
change, manual `date` — cannot make history appear to move backwards or produce a
negative rate.

`SystemSnapshot` is deliberately **not** `Serialize`, because `Instant` has no
meaningful serialized form. Export goes through a dedicated DTO that emits
`wall_time` instead.

## Sampling tiers

Not everything is worth measuring every second, and some things are expensive
enough that measuring them every second would itself distort the measurement.

| Tier | Default | Contents |
|---|---|---|
| Fast | 1 s | CPU, memory, process summary, network counters, disk counters |
| Medium | 5 s | filesystem capacity, static device state, temperatures, battery |
| Slow | 30 s | users, static system metadata, device/interface lists, cgroup metadata |
| On demand | — | selected-process detail, ancestry, open files, socket counts, mount/device mapping |

Filesystem capacity is in the medium tier specifically because a `statfs` on a
stalled NFS mount can block for seconds, and the fast tier must not be able to
stall.

The on-demand tier exists because §2.4's process context is expensive per
process: reading a working directory, counting file descriptors, and walking an
ancestry chain for *every* process on *every* tick would dominate the sample
budget. It is collected for the selected process only.

The UI refresh rate may exceed the data sample rate, but an unchanged frame is
not redrawn without reason.

## Channels and coalescing

The event channel is **bounded**. This is the single most important concurrency
decision: an unbounded channel turns a slow UI into unbounded memory growth, and
the queued snapshots are stale by the time they are read anyway.

When the UI falls behind, snapshots are **coalesced**: the newest supersedes the
older ones, which are counted in `CollectorHealth::coalesced_samples` rather than
silently dropped. A monitor that quietly hides its own lag is lying, so the lag
is displayed in the header once it exceeds one sample interval.

Rules the implementation holds to:

* Only one thread may call crossterm's blocking `poll`/`read` pair. Splitting
  those across threads produces lost and duplicated input.
* Keyboard handling never waits on process enumeration. Input stays responsive at
  10,000 processes because it is on a different thread from sampling.
* A slow detail lookup cannot block regular sampling — that is why the detail
  worker is a separate thread rather than a step in the sampler.
* Every worker gets a shutdown token and is joined on exit. If a worker cannot be
  joined promptly, the terminal is restored *first* and the failure recorded.

## Reducer and effects

```rust
fn reduce(state: &mut AppState, action: Action) -> Effect
```

The reducer is the only thing that mutates application state, and it performs no
side effects. It *returns* them:

```rust
enum Effect {
    None, RequestRedraw,
    FetchProcessDetail(ProcessIdentity),
    SignalProcess { identity: ProcessIdentity, signal: SignalKind },
    ReloadConfig, ExportSnapshot(PathBuf), Shutdown,
}
```

This is what makes the safety-critical paths testable. A test can drive the
confirmation dialog to completion and assert that `Effect::SignalProcess` was
returned, without any process being signalled. Widgets cannot mutate shared state
and cannot signal a process; they emit `Action`s.

Rendering is as close to a pure function of `(state, area)` as ratatui allows. **No
OS call, file write, or collector call happens during render.** A render that
performs I/O is a render that can block the UI thread or panic mid-frame.

## Collector capability states

Platform support is **per metric**, never one global boolean. Every metric an OS
might withhold is wrapped in:

```rust
enum MetricState<T> {
    Available(T),
    Stale { value: T, age: Duration },
    WarmingUp,
    PermissionDenied,
    Unsupported,
    TemporarilyUnavailable(UnavailableReason),
}
```

There is deliberately no API that converts an unavailable metric into a number.
`.fresh()` returns `None` for a stale value too, so staleness cannot leak into a
calculation or become a rate baseline. `.displayable()` returns the value *paired
with its age*, so a retained value cannot be rendered without knowing it is
stale.

`CapabilitySnapshot` records, per metric, whether it is available, unsupported,
permission-denied, or unprobed. The layout engine consults it before reserving
space for an optional panel; the Inspect screen renders the whole list, which is
how a user finds out what their specific machine cannot report and whether
elevated privileges would help.

`PermissionDenied` and `Unsupported` are distinct on purpose: root can grant the
first and cannot conjure the second.

## Time Lens data reduction

Retaining the full process table for every historical sample would cost roughly
`processes × fields × samples` — hundreds of megabytes on a busy machine — and
almost none of it would ever be read.

Instead each `HistoricalSample` holds:

* a **compact aggregate** of the system metrics (CPU, memory share, swap, load,
  aggregate disk and network rates), each still a `MetricState` so an unavailable
  metric stays unavailable in history rather than becoming a spike; and
* a **`ContributorSet`**: the top *K* (default 10) contributors for CPU, resident
  memory, disk read, and disk write, deduplicated by `ProcessIdentity`.

Each contributor keeps its identity, name, a **truncated** command, the absolute
measurement, and the delta or rate. Commands are truncated rather than retained in
full both for memory and because arguments frequently contain secrets.

Each set also carries **evidence coverage**: the share of the observed system
total that the retained contributors account for. This is what makes the
attribution honest — "78% of observed CPU accounted for by retained top
processes" tells the user how much of the picture they are seeing. The wording
throughout is "top contributors" and "correlated with", never "caused by", because
sample correlation is not causation.

Seeking is index arithmetic over a ring buffer, so it is constant time regardless
of history length. The ring is bounded by both sample count and a memory budget;
when configuration would exceed either, it is clamped and the clamping is
*reported* so the UI can warn rather than silently doing something other than
what was asked.

## Terminal restoration

An RAII guard enables raw mode, enters the alternate screen, optionally enables
mouse capture, and hides the cursor — and records **which of those actually
succeeded**. `Drop` undoes exactly what was applied, so a failure partway through
setup does not leave a partially configured terminal, and restoration is
idempotent.

A panic hook attempts restoration *before* printing the panic report, otherwise
the report lands on a terminal still in raw mode with the alternate screen
active. This is why the release profile keeps `panic = "unwind"`: `abort` would
skip the hook and leave the terminal unusable.

The terminal side effects sit behind a trait with a real crossterm implementation
and a recording fake, so setup/restore ordering, idempotency, partial-init
recovery, and the panic path are all unit-tested without a real interactive
terminal.

While the alternate screen is active, nothing is written to stdout or stderr. A
stray `println!` corrupts the display; logging goes to a file or nowhere.

## Process action safety

The whole path is built so that no single mistake sends a signal.

1. `x`, `T`, `K`, `R` produce a *proposal* action. There is a test asserting that
   no key in the entire keymap can yield `Effect::SignalProcess` directly.
2. A confirmation dialog shows the process name, PID, start time or age, user,
   command, the requested action, and its consequences. `SIGKILL` is ordered last
   and visually marked as forceful.
3. The pending action carries a full `ProcessIdentity`, not a PID.
4. Immediately before the signal is delivered, the identity is **re-read from the
   live system**. If the PID now maps to a different start key, the PID was reused
   and the action aborts rather than signalling an unrelated process.
5. Process actions are disabled entirely while inspecting history, because the
   process shown may no longer exist and the displayed state is not current.
6. `EPERM` and already-exited outcomes are reported explicitly. A zombie is not
   signalable and the dialog says so rather than pretending to act.

monitrs never escalates privileges, never invokes `sudo`, never signals a process
tree, and never runs a configured command before or after a signal.

## Performance budgets

These are the targets the implementation is held to. Six of the seven have been
measured; the measurements, the machine, the commands that produce them and the
per-read breakdown are in
[`benchmarks.md`](benchmarks.md#the-161-end-to-end-budgets), which is the file to
trust — the last column here is a summary and will go stale first.

Reference workload: 8 logical CPUs, 200 processes, 80×24 and 160×48 terminals, 1 s
interval, 5 min history. **Nothing has been measured on that workload.** Every figure
below comes from a 12-core Mac running about a thousand processes, which is five times
the process count the budgets assume, and per-process OS reads are where the cost is.

| Budget | Target | Measured (1000 processes, not the reference workload) |
|---|---|---|
| Idle self CPU | median < 1%, p95 < 2% | median 0.5–1.1% — met; **p95 6–11% — fails** |
| Resident memory | < 50 MiB in the default configuration | median 24.5–26.7 MiB, peak 27.2 MiB |
| Input to visible response | < 50 ms when no collector result is needed | median 417 µs, p95 486 µs |
| Frame render at 160×48 | < 16 ms | median 200 µs, p95 353 µs |
| Sample collection at 200 processes | < 200 ms p95 | p95 59–71 ms for a fast-only tick at 1000 processes; 136–149 ms when the medium tier joins |
| 12-hour run | no unbounded memory or file-descriptor growth | 30 minutes: no growth. The 12-hour run is still owed |
| Redraw | no busy loop | not measured as such |

The idle-CPU failure is not in monitrs' own computation, which is about 35 µs per
tick — three orders of magnitude below the OS reads that surround it. `benchmarks.md`
locates it read by read and states what would close it.

Under high load — 10,000 processes, or a stalled `/proc` — the required behaviour
is: input stays responsive, snapshots coalesce, collector lag is *displayed*,
expensive enrichment is progressively reduced, queues never grow without bound,
and sorting or filtering never freezes the terminal. Any bound applied under load
is surfaced rather than silently truncating.

monitrs measures its own overhead and shows it on the Inspect screen. A system
monitor that hides its own cost is not trustworthy.
