# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries describe what a *user* can observe. Anything not listed here does not
work yet, regardless of what the source contains.

## [Unreleased]

### Added

* Cargo workspace with four crates and a one-directional dependency graph:
  `monitrs-core` depends on no terminal library and no OS collector.
* Platform-neutral data model: `SystemSnapshot` and its component snapshots for
  host, CPU, memory, processes, disks, filesystems, networks, pressure, sensors,
  capabilities, and collector health.
* `MetricState<T>` per-metric availability, so an unavailable metric can never be
  rendered as zero. Includes a `Stale` variant that cannot be read without its
  age.
* `ProcessIdentity` pairing a PID with a platform start key, so PID reuse is
  detectable by construction.
* Validated `Percent` and `Rate` scalars that reject NaN, infinities, and
  negatives at construction, and byte/duration/text formatters whose output is
  bounded by a display-width budget it can never exceed.
* Quality gates: rustfmt, a clippy policy, `cargo deny`, and a CI matrix covering
  Linux glibc and musl on x86_64 and aarch64, macOS on both architectures, an
  MSRV job, and an assertion that exactly one `crossterm` major version resolves.
* Reusable widgets — meter, sparkline, per-core strip, panel, process table, tree
  prefixes, pressure radar, pinned strip — with 23 snapshot tests covering ASCII,
  Unicode, and no-colour modes, plus the empty, permission-denied, stale, and
  warming-up states. A painter that clips every write, so no widget can draw
  outside its area or panic on a zero-area one.
* Application state and the reducer: stable selection by process identity, pinning,
  Time Lens pause/seek/return-to-live, and a confirmation chain in which
  `Effect::SignalProcess` has exactly one constructor, reachable only after an
  accepted confirmation.
* Pressure engine and the diagnostic rules of §11.2, with hysteresis that a test
  proves does not flap on alternating input, and a test asserting no rule claims
  OOM, a memory leak, disk failure, malware, or thermal throttling.
* Linux `/proc` and `/sys` enrichment: PSI, cgroup v2 limits reported separately
  from host totals, device busy time, per-process I/O, and a start key with
  clock-tick resolution. Every parser takes bytes rather than a path, so all 120
  fixtures — including a process name containing spaces and parentheses, truncated
  reads, counter resets, and the cgroup `max` sentinel — run on every platform.
* macOS native enrichment through documented APIs only: `sysctl`,
  `host_statistics64`, `host_processor_info`, `proc_pidinfo`, `getifaddrs`, and
  IOKit's public power interfaces. Wired and compressed pages reported separately
  per §8.4. No external commands, no private APIs, and a `SAFETY:` comment on every
  unsafe block.
* CLI, JSON snapshot export with argument redaction, versioned TOML configuration
  with `config path`/`init`/`check`, the bounded event channel and worker threads,
  and benchmarks with measured results in `docs/benchmarks.md`.


### Changed

* Relicensed from GPL-3.0 to dual MIT OR Apache-2.0, matching the Rust ecosystem
  norm and the project's dependency license policy.

[Unreleased]: https://github.com/gaborini/monitrs/commits/main
