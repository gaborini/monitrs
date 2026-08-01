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

## What 1.0.0 froze

`1.0.0` is a stability promise, and these are its terms.

**API.** The public items of `monitrs-core`, `monitrs-collectors` and
`monitrs-tui` — the three library crates in the dependency graph above.
`monitrs` itself ships no library target (`crates/monitrs/Cargo.toml` declares a
`[[bin]]` and no `[lib]`), so it has no API to freeze; its observable behaviour is
covered by the export, configuration and keymap promises below instead. Checked
by the `semver compatibility` CI job (`.github/workflows/ci.yml`), which compares
the workspace against the last version published to crates.io.

**Right now that job reports and does not block.** It runs with
`continue-on-error: true`, because there is no published 1.x release for the
workspace to be compatible against yet, so a failure would be noise rather than
information. **At the tag it becomes the gate:** the commit that tags `v1.0.0`
flips `continue-on-error` to `false` and deletes the comment above it in
`ci.yml`, and from that point a breaking change not paired with a major bump
fails CI. Until that flip happens, the freeze described below is a policy with no
mechanism behind it — which is why the release checklist carries the flip as a
step of its own.

**Seven enums are `#[non_exhaustive]` on purpose: `Effect`, `Action`, `ViewId` and
`SortField` (`crates/monitrs-tui/src/action.rs`), the palette `Command`
(`crates/monitrs-tui/src/app/command.rs`), `HistoryMetric`
(`crates/monitrs-core/src/history/sample.rs`), and `FilterPattern`
(`crates/monitrs-core/src/process/filter.rs`).** The first six were marked going
into 1.0.0; `FilterPattern` was already marked long before, for the same reason
stated differently — it has exactly one variant today and §7.2 leaves room for a
second, so the attribute is what keeps adding one from being a major bump. These
are the enums the roadmap
expects to grow within 1.x — a new effect, a new screen, a new sortable column, a
new palette command, a new retained metric — and without the attribute, each
addition would need a major bump the way `Effect::SetSensorInterest` did going
into 1.0.0. `MetricState` (`crates/monitrs-core/src/model/metric.rs`) is
deliberately **not** on this list: there, a consumer's exhaustive match is the
protection, not an inconvenience, and a new availability state should cost a
major bump because every one of them must be handled deliberately. Marking an
enum `#[non_exhaustive]` is itself a breaking change — matching it from another
crate now requires a wildcard arm — so it can only happen before a major
release, never inside one; do not add the attribute to an eighth enum without
the same deliberation this list got. **The cost lands in
`crates/monitrs/src/interactive.rs`'s `execute`:** its match on `Effect` can no
longer be proven exhaustive by the compiler, so **adding an `Effect` variant now
requires adding its arm to `execute` by hand** — an effect with no arm falls
through to the wildcard, which logs an error and continues rather than doing
nothing silently, but a logged bug in production is still a bug; add the arm.

**The JSON export.** `docs/schema/v2.json` lists every field path version 2
produces, and `crates/monitrs/tests/schema_contract.rs` fails if one disappears.
Adding a field is not a break. Removing one, or changing what one means, is a
`schema_version` bump with a new `docs/schema/v{N+1}.json` written beside the old
one — the old file stays, so a consumer can see exactly what changed between the
two.

**Configuration keys.** `docs/schema/config-v1.json` lists every key path a
configuration file may contain, and `crates/monitrs/tests/config_contract.rs`
fails if one of them stops existing — derived by serialising a `Config` and
walking it, so the keys come out of the serde structs rather than out of a second
list. Adding a key is not a break; removing or renaming one is a `config_version`
bump with a new `docs/schema/config-v{N+1}.json` beside the old file. Note what
`#[serde(deny_unknown_fields)]` does and does not do: it rejects a key the *user*
invented, and it cannot notice monitrs itself deleting one. The same test also
asserts that `known_keys()` — the hand-maintained typo-suggestion list in
`config.rs` — still matches the structs, because a suggestion list that has
drifted points at a key the parser will reject next.

**The default keymap.** `every_global_binding_from_the_spec_resolves_in_normal_mode`
and `every_list_binding_from_the_spec_resolves_in_normal_mode`
(`crates/monitrs-tui/src/keymap.rs`) assert exact key→action pairs, so a rebound
default key fails a test rather than a bug report. A binding may be added; an
existing one does not change meaning without a major bump.

**Four guards, four surfaces, and no guard covers another's blind spot.** Three of
them fail a build today; the semver job is the one that only reports until the tag
flips it, as described above.
`cargo-semver-checks` reads the Rust API; it cannot see a `#[serde(rename)]`,
which reshapes the JSON export while leaving every Rust signature untouched. The
schema inventory reads the JSON wire format; it cannot see a Rust signature
change that never reaches serialisation. The configuration inventory reads TOML
key paths; it sees neither of those, and neither of them sees a deleted
configuration key. The keymap tests read the built-in keymap and nothing else. A
renamed export field is caught only by the schema test; a removed public function
only by the semver job; a renamed configuration key only by the configuration
test; a rebound `q` only by the keymap tests. Treat them as complementary, not
redundant — dropping any one reopens a different way to break the freeze
silently.

**What none of the four can see is a *meaning* that changes while every name stays
put.** `sampling.slow_interval` taking over the sensor read from
`sampling.medium_interval` going into 1.0.0 is the worked example: no key was
added, removed or renamed, so no inventory moved, and the only record of it is
`docs/configuration.md`. Changing what a frozen key or field *does* is therefore
a documentation change as much as a code change, and the documentation is the
part a reviewer has to check by reading.

**Not frozen:** layouts, wording, colours, glyph choices and panel arrangement.
These are presentation. A cosmetic improvement is not a breaking change, and this
paragraph exists so nobody has to guess which side of the line a change is on.

**MSRV.** Raising it is a minor bump. `Cargo.toml`'s `rust-version` is the
record; `rust-toolchain.toml` pins the development channel and says nothing about
the minimum — that's why the `msrv` CI job sets `RUSTUP_TOOLCHAIN` explicitly
rather than trusting the toolchain file to reflect it.

**Deprecation.** A deprecated item lives at least one minor cycle before
removal, and its deprecation message names the replacement.

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
