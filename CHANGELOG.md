# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries describe what a *user* can observe. Anything not listed here does not
work yet, regardless of what the source contains.

## [Unreleased]

Nothing yet.

## [0.1.0] - 2026-07-30

First release. `monitrs` shows what a machine is doing now and what it was doing a
few minutes ago, and it is explicit about the difference between a metric it
measured, one it is still warming up, one the OS refused, and one this platform
cannot produce.

### Added

#### The interface

* **Five screens** — Overview, Processes, Storage, Network, Inspect — and six
  overlays: help, command palette, filter edit, sort selector, process detail, and
  the process-action confirmation that covers both signals and renice. Notices are a
  panel rather than an overlay, and the spike attribution is the body of the Time Lens
  screen rather than something you open. Keyboard-first throughout; the mouse is
  optional and off by default.
* **The Time Lens.** `Space` pauses the visible timeline without stopping
  collection, `[` and `]` scrub through the retained history, and `L` returns to
  live. Selecting a historical sample shows which processes accounted for the
  change, with an explicit statement of how much of the change the named processes
  account for — correlation, described as correlation.
* **The Pressure Radar.** Each signal carries the rule text that produced it, the
  raw metric it was derived from, and how long the state has held. Hysteresis stops
  it flapping. No rule claims a diagnosis the data cannot support: no OOM, no memory
  leak, no disk failure, no malware, no thermal throttling.
* **A stable process table.** Sorting is total, with a `(pid, start key)`
  tie-breaker, so a hundred rows all reporting `0%` keep their order between
  refreshes. The cursor follows the process you chose rather than the row it was
  on; until you choose one it tracks the top of the table, so the busiest process is
  what a fresh session shows. Tree mode re-attaches the children of a filtered-out
  parent instead of scattering them.
* **Four layout bands** from 160×48 down to 80×24, and below that a minimal process
  list rather than a broken frame. Every panel's width is reserved from the layout,
  so no value can push a column out of alignment.
* **Strict ASCII mode** (`--ascii`) whose output is seven-bit by assertion, a
  Unicode mode, three themes, and `--color off`/`NO_COLOR`. Colour is never the only
  carrier of meaning: every state also has a distinct character, and every
  unavailable value says which kind of unavailable it is — `warming up`,
  `permission denied`, `n/a`, or a specific reason — abbreviated but never merged
  as the column narrows.

#### What it measures

* **Per-metric availability.** `MetricState<T>` makes "no value" a state rather
  than a zero, and a retained value cannot be displayed without its age.
* **Native enrichment by default.** On Linux, `/proc` and `/sys`: PSI, cgroup v2
  limits reported separately from host totals, device busy time, per-process I/O,
  and a start key with clock-tick resolution. On macOS, documented APIs only —
  `sysctl`, `host_statistics64`, `host_processor_info`, `proc_pidinfo`,
  `getifaddrs`, and IOKit's public power interfaces — with wired and compressed
  pages reported separately. No external commands and no private APIs on either.
* **Rates are computed from measured intervals**, never from an assumed one, and a
  counter that went backwards reports a reset rather than a spike.
* **Process identity is a PID plus a start key**, so a reused PID inherits nothing:
  not the selection, not a pin, and not a pending action.

#### Doing something about it

* **Signals** — `x` opens the dialog, `T` proposes SIGTERM, `K` proposes SIGKILL —
  behind a confirmation that names the process. A signal never follows from one
  keypress, and the forceful ones demand a distinct key rather than `Enter`, so
  leaning on the confirm key cannot escalate. The identity is rechecked immediately
  before delivery, so a PID reused between the dialog and the write is refused
  instead of signalled.
* **Renice** (`R`) on both platforms, with the same revalidation. A dry run says in
  advance whether the value will be permitted; lowering a nice value needs
  privileges monitrs never acquires, and it says so rather than failing silently.
* **Process actions are refused while the timeline is away from live**, because the
  process on a frozen screen is not necessarily the process that PID names now.
* **No privilege escalation, ever.** monitrs never invokes `sudo`, never asks for a
  password, and reports what it could not read.

#### Around the edges

* `monitrs snapshot --format json` with command arguments redacted by default,
  `monitrs config path`/`init`/`check`, shell completions for five shells, and a
  manpage — the last two generated from the same definition the program parses.
* **Versioned TOML configuration** that rejects an unknown key rather than ignoring
  it, points at the exact key for an out-of-range value, suggests the near miss for
  a typo, and detects key conflicts. Reload is atomic and applies to the running
  interface *and* the sampler; the settings that need a restart are named.
* `--debug-log` on every subcommand, carrying collector durations and the
  dropped/coalesced counts, and never writing to a terminal that is showing the
  interface.
* **The terminal is restored** on quit, on `Ctrl-C`, on `SIGTERM`, on `SIGHUP`, on
  error, and on panic — before the panic report is printed, and before slow workers
  are joined. A signal reaches the ordinary shutdown path rather than killing the
  process where it stands, so `kill` gives the terminal back instead of leaving it
  needing `reset`. Verified on a real pty by
  `scripts/verify-terminal-restoration.py`, which checks the escape sequences *and*
  the pty's `termios` state for each case.

### Measured

§16.1's budgets, measured rather than asserted. Frame render, input latency and
collection come from `crates/monitrs/tests/capture.rs`; self CPU, resident memory
and descriptors from `scripts/measure-overhead.py`, which observes the running
binary from outside. Full numbers and the per-read breakdown are in
[`docs/benchmarks.md`](docs/benchmarks.md#the-161-end-to-end-budgets).

| Budget | Measured on a 12-core Mac, ~1000 processes |
|---|---|
| frame render below 16 ms at 160×48 | median 200 µs, p95 353 µs |
| input-to-visible-response below 50 ms | median 417 µs, p95 486 µs |
| sample collection below 200 ms p95 | ordinary tick p95 15–21 ms; every fifth tick 121–161 ms |
| resident memory below 50 MiB | median 29 MiB, peak 31 MiB |
| no unbounded growth | 30-minute soak: resident size *fell*, descriptors flat, nothing dropped |

2224 tests. Among them: 86 recorded frames of the rendered interface across three
snapshot suites, covering ASCII, Unicode, no-colour, and the empty,
permission-denied, stale and warming-up states; 120 Linux `/proc` and `/sys` fixtures
that run on every platform because each parser takes bytes rather than a path;
property tests over the formatters and the sort; 13 integration tests over the
assembled application; and a soak harness that drives the real worker threads.

### Known limitations

Named because §16.1's own last line asks for measurement rather than claims:

* **The idle self-CPU budget is not met.** Median 1.3–2.7% against a 1% target, and
  p95 11–15% against 2%, on a host with about a thousand processes — five times
  §16.1's reference workload. The cost is OS reads, not monitrs' own computation,
  which is three orders of magnitude smaller; `docs/benchmarks.md` locates it read by
  read and says what would close it.
* **No twelve-hour soak is on record.** A 30-minute run with the shipped collector
  is, and shows no growth. The twelve-hour run is the actual gate, and nothing has
  been soaked on Linux.
* A slow terminal can block a frame for an unbounded time, and no instrument here can
  see it: the frame-time measurement renders through ratatui's `TestBackend`, which
  stops short of the write to the terminal, and the soak harness has no renderer.
* Per-process socket counts and device busy time are unsupported on macOS: the first
  costs one syscall per descriptor, the second has no documented API. Both say
  `n/a` rather than guessing.
* PSI is Linux-only, and says `n/a` on macOS rather than promising a value that will
  never arrive.
* Timestamps are UTC and labelled `Z`: no time-zone database is bundled.
* Of the six published archives, only the two macOS ones have been run — and the
  x86_64 one only under Rosetta on Apple Silicon, where it reports temperatures as
  unsupported. Every archive's checksum and build attestation verifies; nobody has run
  the four Linux archives or an Intel Mac build on its own hardware.

### Changed

* Relicensed from GPL-3.0 to dual MIT OR Apache-2.0 before any release, matching
  the Rust ecosystem norm and this project's dependency licence policy. Anyone who
  cloned the repository at its first commit saw the earlier licence.

[Unreleased]: https://github.com/gaborini/monitrs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/gaborini/monitrs/releases/tag/v0.1.0
