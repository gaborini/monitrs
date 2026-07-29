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

### Changed

* Relicensed from GPL-3.0 to dual MIT OR Apache-2.0, matching the Rust ecosystem
  norm and the project's dependency license policy.

[Unreleased]: https://github.com/gaborini/monitrs/commits/main
