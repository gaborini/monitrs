# Contributing to monitrs

Thanks for wanting to help. This document is short on ceremony and specific about
the two things monitrs is strict about: **metric honesty** and **process-action
safety**.

## Getting set up

```sh
git clone <your fork>
cd monitrs
cargo test --workspace
```

The toolchain is pinned to stable in `rust-toolchain.toml`. Nothing else is
required. `just` is optional; every recipe is a one-line wrapper and `just
--list` prints the underlying command.

Before opening a pull request:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

or `just ci`, which runs exactly what CI runs, in CI order.

## The two rules that are not negotiable

### 1. Unavailable is not zero

A system monitor that shows `0` for something it could not measure is worse than
one that shows nothing, because it is confidently wrong.

Every metric an OS might withhold is wrapped in `MetricState<T>`. There is
deliberately no API that converts an unavailable metric into a number. When you
add a metric:

* Missing because the platform has no such concept → `Unsupported`
* Missing because the OS refused → `PermissionDenied`
* Missing because it needs two samples → `WarmingUp`
* Missing for a transient, nameable reason → `TemporarilyUnavailable(reason)`
* Showing a previous value → `Stale { value, age }`, and it must render with a
  visible marker and its age

Related: rates must divide by the *actual* elapsed interval, never an assumed
one. A counter that moves backwards produces `CounterReset` for that sample, not
a spike. And nothing may claim a process *caused* a system event — the wording is
"top contributors" or "correlated with".

### 2. No signal from a single keypress

`x`, `T`, `K`, and `R` open a confirmation. They never send anything. The
confirmation shows the process name, PID, start time, user, command, the
requested action, and its consequences.

Immediately before a signal is delivered, the process identity is re-read. If the
PID now refers to a different start key, the action aborts — the PID was reused
and signalling it would hit an unrelated process.

Process actions are disabled entirely while inspecting history.

There is a test asserting that no key in the entire keymap can produce a signal
effect directly. If your change makes it fail, the change is wrong.

## Architecture in one paragraph

Four crates with a strictly one-directional dependency graph:
`monitrs-core` (data model, rates, history, diagnostics — no terminal, no OS
collector) ← `monitrs-collectors` (sysinfo baseline plus native Linux/macOS
enrichment) and `monitrs-tui` (ratatui rendering, reducer, keymap, layout), both
of which the `monitrs` binary wires together.

The TUI must not know how `/proc` is parsed. Collectors must not know how a
process row is coloured. Rendering is a function of state and terminal area:
**no OS calls, file writes, or collector calls happen during render.** The reducer
returns `Effect`s; the runtime performs them. That is what makes safety dialogs
testable without sending real signals.

See [`docs/architecture.md`](docs/architecture.md) for the full picture, and
[`docs/adr/README.md`](docs/adr/README.md) for why things are the way they are.

## Testing expectations

Name tests after the behaviour they pin down, not after the function they call.
`unavailable_is_never_zero` tells a future reader what broke; `test_metric_state`
does not.

* **Parsers** take a byte slice or `Read`, never a path, so they are testable
  from fixtures with no live filesystem. Add a sanitized fixture for every new
  format, including a malformed and a truncated variant.
* **UI** changes need `insta` snapshots for the states they affect — including
  the empty, permission-denied, warming-up, and stale states, not only the happy
  path.
* **Reducer** changes assert both the next state and the emitted effects.
* **Platform smoke tests** are `#[ignore]`d so `cargo test` stays hermetic; CI
  runs them on real Linux and macOS runners.
* Do not assert exact CPU utilization values in a test that runs on real
  hardware. They are not reproducible.

## Style

* Every public item gets a doc comment. Explain *why*, especially where a rule is
  non-obvious; cite the reasoning rather than restating the signature.
* Comments earn their place. A comment that says what the next line does is
  noise; one that says why the obvious approach was rejected is valuable.
* No `unwrap`, `expect`, or panicking index outside `#[cfg(test)]`. A panic in a
  TUI leaves the user's terminal broken. Clippy enforces this.
* `unsafe` is forbidden in `monitrs-core` and `monitrs-tui`. In the platform
  collectors it is allowed only where FFI requires it, must be as small as
  possible, and every block needs a `SAFETY:` comment stating the invariant that
  makes it sound. Clippy denies undocumented unsafe blocks.
* Raw counters and timestamps are integral. Floating point is for validated
  calculated rates and percentages only.
* Prefer a small number of threads and bounded channels. Do not add an async
  runtime; `cargo deny` rejects one.

## Adding a dependency

Dependencies are not free. Before adding one:

* Is it maintained, and does it have a license compatible with dual MIT/Apache-2.0
  distribution? `cargo deny check` will tell you.
* Can default features be turned off?
* Does something already in the graph do this?
* If it is unmaintained or duplicates an existing crate's function, it needs a
  decision record in `docs/adr/`.

## Out of scope for v1

Please do not open pull requests for these; they will be closed with thanks but
without merging. Windows support, a resident daemon, persisting history across
launches, a database, eBPF in the default binary, packet capture or per-process
network attribution, executing shell commands from configuration, private or
undocumented macOS APIs in the default build, killing process trees with one
keypress, plugins before the core contracts are stable, and animations that cost
CPU or reduce legibility.

## Conduct

Be decent to each other. See [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

## License

By contributing you agree that your work is dual-licensed under MIT and
Apache-2.0, matching the project.
