# monitrs

**A fast, keyboard-first system cockpit for Linux and macOS, built in Rust.**

<p align="center">
  <img src="https://raw.githubusercontent.com/gaborini/monitrs/main/docs/demo/monitrs.gif"
       alt="monitrs running: the Overview screen with live meters, the pressure radar, history sparklines and the process table">
</p>

<p align="center">
  <a href="https://crates.io/crates/monitrs"><img
    src="https://img.shields.io/crates/v/monitrs?style=flat-square" alt="crates.io"></a>
  <a href="https://github.com/gaborini/monitrs/actions/workflows/ci.yml"><img
    src="https://img.shields.io/github/actions/workflow/status/gaborini/monitrs/ci.yml?branch=main&style=flat-square&label=ci" alt="CI"></a>
  <a href="https://docs.rs/monitrs-core"><img
    src="https://img.shields.io/docsrs/monitrs-core?style=flat-square&label=docs" alt="docs.rs"></a>
  <img src="https://img.shields.io/crates/msrv/monitrs?style=flat-square" alt="MSRV">
  <img src="https://img.shields.io/badge/platform-linux%20%7C%20macos-informational?style=flat-square" alt="platforms">
  <img src="https://img.shields.io/crates/l/monitrs?style=flat-square" alt="licence: MIT OR Apache-2.0">
</p>

monitrs shows you what your machine is doing right now — and, unlike most
terminal monitors, what it was doing thirty seconds ago. Pause the timeline,
scrub back to a spike, and see which processes were most strongly correlated
with it.

> **Status: `1.0.1`, a stability promise with a machine behind it.** It is on
> [crates.io](https://crates.io/crates/monitrs) and
> [GitHub](https://github.com/gaborini/monitrs/releases/tag/v1.0.1). Four surfaces are
> frozen — the public API of the three library crates, the JSON export, the configuration
> keys, and the default keymap — and each has a guard that fails a build rather than a
> paragraph that asks nicely: the inventories in [`docs/schema/`](docs/schema/) with the
> contract tests that read them, the keymap's own tests, and `cargo-semver-checks` for
> the API, which this release's own commit turned from advisory into a gate.
> [`CONTRIBUTING.md`](CONTRIBUTING.md) states the terms. **What is not frozen is how it
> looks**: layout, wording, colour, glyph choice and panel arrangement are presentation,
> and a cosmetic change is not a breaking one.
>
> **The twelve-hour soak `1.0.0` deferred has been run, and it passes.** Twelve hours on
> both Tier 1 Linux architectures, plus an hour at 10,000 processes and an hour on the
> real collector: resident memory moved 593 KiB and 115 KiB against a 16 MiB allowance,
> the history ring held one size throughout and descriptors held at 4. The first attempt
> failed and the failure was in this repository's own test harness, which had been keeping
> every simulated keypress and reporting its own 40 MiB as monitrs'; `1.0.1` fixes that
> too.
>
> **The caveat that survives the `1.0` label**, because relabelling it would not fix it:
> §16.1's idle self-CPU budget is not met, and on the reference workload the budget names
> — 8 CPUs, 200 processes — **neither half of it is**: median 2.66% against 1% and p95
> 3.99% against 2%, measured on both Tier 1 Linux architectures. On a 12-core Mac with
> a thousand processes the median passes at 0.60–0.85% and the p95 fails at 4.30–9.50%;
> fewer processes did not make it cheaper. Those readings are also quantised in 1.33%
> steps — one scheduler tick per sample — so a passing median is not a value the
> measurement can report at all. The table below carries the figures;
> [`CHANGELOG.md`](CHANGELOG.md)'s *Known limitations* carries the rest, and says —
> because it is the useful part — that the cause `1.0.0` was built around turned out to
> be the wrong one. Where a claim has a caveat, the caveat is next to it rather than left
> out.

## The screens

Seven, on the digit keys, plus six overlays — help, command palette, filter edit, sort
selector, process detail, and the confirmation that covers both signals and renice.

| | | |
|---|---|---|
| `1` | **Overview** | meters, the Pressure Radar, the history sparklines, the process table, pins, and a per-interface network footer |
| `2` | **Processes** | the full table, flat or as a tree, with the pinned strip above it |
| `3` | **CPU** | per-core meters grouped by core class, the load average per CPU, and the processes accounting for it |
| `4` | **Storage** | filesystem capacity and inodes, per-device throughput, the processes doing the I/O, and a throughput history |
| `5` | **Network** | per-interface counters, errors, link state, and utilization where the link speed is known |
| `6` | **Inspect** | every fact about the machine and the selected process, plus what this build cannot measure and why |
| `7` | **Battery** | charge, wear against design capacity, draw, and the thermal sensors |

The recording above is a plain screen capture with nothing substituted. Plain-text
frames of every screen except Network live in
[`docs/screenshots/`](docs/screenshots/): Overview in both ASCII and Unicode, CPU,
Storage, Inspect and Battery at 160×48, and Processes in the compact 80×24 layout.
They are written straight out of the renderer with live data by
`crates/monitrs/tests/capture.rs`
(`cargo test -p monitrs --release --test capture -- --ignored`), so nothing in this
repository is a mock-up and no frame can drift from what monitrs actually draws. Only
the hostname and the login name in them are substituted — every measurement, process
name and state is exactly as rendered, which is also why the recording and the frames
show different host names.

## What makes it different

monitrs is not a reimplementation of `htop`. It is built around one idea the
others do not have.

**Time Lens.** A bounded in-memory history — five minutes by default — that you
can pause and scrub. Select a spike and monitrs shows the processes that
contributed most to *that sample*, with an explicit statement of how much of the
observed total those processes account for. It calls this evidence, not proof,
because sample correlation is not causation.

**Honesty about what the OS will not tell you.** Every metric carries its own
availability. A number monitrs cannot measure renders as `warming up`,
`permission denied`, `n/a`, or a named transient reason — never as `0`. A
retained value is marked stale and shows its age. Network utilization is simply
absent when the link speed is unknown, because a percentage of an unknown
capacity is meaningless.

**Pressure Radar.** Pressure signals that each show the raw metric, its
normalized severity, **and the rule that produced the state**, so you never have
to guess why something turned amber. Linux PSI where available.

**Follow a process with its children.** `F` scopes the table to one process and
everything beneath it, and the panel says what the *family* costs:
`4 of 10 total, cpu >=107%, rss 479M`. A build's compilers come and go every
second, so no single row ever answers "what is this build using", and filtering on
`cc` finds every compiler on the machine instead of the four in this build. `>=`
means a member's CPU was refused and the total is a lower bound rather than the
answer.

**Container-aware, in the direction that matters.** Inside a cgroup, monitrs shows
the ceiling that applies *beside* the host's figures — `8 logical, 8 physical,
cgroup 1.5 CPUs`, `cgroup limit 2.0G, 512M used (25%)` — and takes both halves of
that memory ratio from the group, because `/proc/meminfo` is not namespaced and the
host's `used` over a container's limit reports 2000%. It also refuses the tempting
version of this: the load average is *not* divided by the quota, because
`/proc/loadavg` counts every process on the machine including other tenants'.

**Native detail on top of a portable baseline.** monitrs starts from the
cross-platform collector and enriches it through the OS's own interfaces, which is why
per-process thread counts, wired and compressed memory, and the thermal sensors are
numbers here rather than blanks. Enrichment only ever upgrades: a native read that
fails leaves the baseline value it could not improve, so it can never make a metric
worse than the portable path alone.

**Readable without color.** Every state has a redundant ASCII symbol (`.` `!`
`X` `?`). A strict 7-bit ASCII mode renders correctly on any terminal, over any
SSH session, in any locale.

### Compared with the tools it learns from

These are all good programs, and monitrs borrows liberally from what they got
right.

| | monitrs | `htop` | `btop` | `bottom` |
|---|---|---|---|---|
| Scrub back through history | yes | no | no | graphs only |
| Per-sample spike attribution | yes | no | no | no |
| Sum one process tree's cost | yes | no | no | no |
| Explicit per-metric availability | yes | partial | partial | partial |
| Shows the rule behind a warning | yes | n/a | n/a | n/a |
| cgroup ceiling beside the host's | yes | partial | no | no |
| Strict 7-bit ASCII mode | yes | yes | no | partial |
| Process tree | yes | yes | yes | yes |
| Themes and mouse support | yes | yes | yes | yes |
| Windows | no | no | yes | yes |

If you want a mature, universally available process viewer, `htop` remains an
excellent answer. monitrs is for the moment when you ask "what just happened?"

## Supported platforms

| OS | Architecture | Tier | Expectation |
|---|---|---|---|
| Linux (glibc) | x86_64 | 1 | Full support |
| Linux (glibc) | aarch64 | 1 | Full support |
| macOS | arm64 | 1 | Full support |
| macOS | x86_64 | 1 | Full support |
| Linux (musl) | x86_64 | 2 | Best-effort static binary |
| Linux (musl) | aarch64 | 2 | Best-effort static binary |

Windows is not supported and is not planned for v1.

Support is tracked **per metric**, not per platform. See
[`docs/platform-support.md`](docs/platform-support.md) for which metrics each
platform provides, and the Inspect screen (`6`) for what your specific machine
provides right now.

## Install

### From Homebrew

```sh
brew tap gaborini/monitrs
brew trust --formula gaborini/monitrs/monitrs
brew install monitrs
```

All three lines are needed: Homebrew 6 ignores a third-party tap until it is trusted, and
`brew install` without the middle line fails. The formula is a binary one, so this needs no
Rust toolchain — it installs the same archive as the section below, plus the manpage and the
bash, zsh and fish completions. It lives in
[gaborini/homebrew-monitrs](https://github.com/gaborini/homebrew-monitrs) rather than
homebrew-core, whose notability bar for a self-submission is 225 stars.

### From crates.io

```sh
cargo install monitrs --locked
```

### From a checkout

Also what every measurement in this README was taken from:

```sh
cargo build --release
./target/release/monitrs
```

`q` quits. See [`docs/troubleshooting.md`](docs/troubleshooting.md) if a number
surprises you.

### From a release archive

[The `v1.0.1` release](https://github.com/gaborini/monitrs/releases/tag/v1.0.1) has one
archive per target — `x86_64` and `aarch64` for Linux glibc, Linux musl, and macOS —
each carrying the binary, both licences, this README, the changelog excerpt for that
version, shell completions for bash, zsh, fish, PowerShell and elvish, and a manpage.
Installing one means verifying it and putting the binary somewhere on your `PATH`:

```sh
# Replace the version and target with the archive you downloaded.
# The release carries one SHA256SUMS for all six archives, not a file per archive, so
# --ignore-missing is what lets you check the one you actually downloaded.
shasum -a 256 --check --ignore-missing SHA256SUMS     # sha256sum on Linux
tar xzf monitrs-1.0.1-aarch64-apple-darwin.tar.gz
install -m 755 monitrs-1.0.1-aarch64-apple-darwin/monitrs ~/.local/bin/monitrs
```

Releases also carry a build attestation, so `gh attestation verify
monitrs-*.tar.gz --repo gaborini/monitrs` confirms the archive came from this
repository's workflow rather than from someone else.

Honest caveat: all six published archives have had their checksums and build
attestations verified, but only the two macOS ones have been *run* — and the x86_64 one
only under Rosetta on Apple Silicon, where it reports temperatures as unsupported (see
[`docs/platform-support.md`](docs/platform-support.md)). Nobody has run the four Linux
archives or an Intel Mac build on its own hardware.

## Keys

Full, always-current help is generated from the live keymap: press `?`.

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit |
| `?` | Context-aware help |
| `1`–`7` | Overview / Processes / CPU / Storage / Network / Inspect / Battery |
| `Tab`, `Shift-Tab` | Next / previous panel |
| `j` `k`, `Down` `Up` | Next / previous row |
| `Ctrl-D` `Ctrl-U` | Page down / up |
| `gg` `G`, `Home` `End` | First / last row |
| `Space` | Pause or resume the visible timeline |
| `[` `]` | Step back / forward one sample |
| `{` `}` | Leap back / forward through history |
| `L` | Return to live |
| `/` | Filter |
| `n` `N` | Next / previous match |
| `s` `S` | Sort selector / reverse sort |
| `f` | Toggle flat and tree view |
| `F` | Follow the selected process tree |
| `p` | Pin or unpin the selected process |
| `Enter` | Inspect the selected item |
| `x` | Signal dialog for the selected process |
| `y` | Confirm the pending action |
| `Y` | Confirm a *forceful* action — `SIGKILL` accepts only this |
| `:` | Command palette (§6.3) |
| `t` `g` | Cycle theme / glyph mode |
| `r` | Force refresh |
| `Esc` | Close overlay or cancel |

The palette is not only a second way to press a key. Thirteen commands live there, and
three of them have no key at all: `follow <pid>` and `unfollow` for the subtree scope, and
`export snapshot <path>`. The others set the view, sort, filter, sample interval, history
span, theme, glyphs and colour depth, show the configuration path, and reload it. Type `:`
and the list appears; it narrows as you type and completes towards the highlighted entry.

`T`, `K`, and `R` *propose* SIGTERM, SIGKILL, and renice. None of them acts on a
single keypress; each opens a confirmation showing the process identity and the
consequences, and the forceful ones want `Y` rather than the `y` that confirms everything
else, so leaning on the confirm key cannot escalate. The identity is rechecked immediately
before the write: a PID that was reused between the dialog and the confirmation is
refused rather than acted on.

## Configuration

monitrs is useful with no configuration and **does not create a config file on
first launch**.

```sh
monitrs config path     # where it looks
monitrs config init     # write a documented starter file (never overwrites)
monitrs config check    # validate without launching
```

CLI flags override file values. See
[`docs/configuration.md`](docs/configuration.md) for every key — including
`diagnostics.bell_on_critical`, off by default, which rings the terminal bell once when a
pressure signal escalates into critical.

## Scripting: one snapshot, as JSON

```sh
monitrs snapshot --format json
```

Takes one sample, prints it, exits. Every metric carries its own availability, so a field
your machine cannot produce reads `"unsupported"` or `"permission_denied"` rather than `0` —
the same rule the interface follows, in a form a script can branch on.

The payload starts with `"schema_version"`, and that number is a promise: it is bumped
whenever a field is **removed** or its meaning changes, so a consumer can refuse an export it
does not understand instead of misreading it. It is `2` as of 0.2.0, and
[`CHANGELOG.md`](CHANGELOG.md) says exactly which fields moved and why. Command lines are
redacted by default; nothing in the export contains an environment variable, because the
model has no field for one.

## A warning about metrics

Memory, CPU, and disk numbers do not mean the same thing on Linux and macOS, and
monitrs will not pretend they do.

* Linux `used` memory is `total - MemAvailable`; page cache is **not** counted as
  application use. macOS reports wired and compressed pages separately, because
  neither is reclaimable the way Linux page cache is.
* Process CPU defaults to *one core = 100%*, so a process using four cores reads
  `400%`. Switch with `process_cpu_normalization = "machine"`.
* Filesystem capacity and device utilization are different metrics and are never
  combined into one percentage.
* Percentages, rates, and pressure states are defined in
  [`docs/metrics.md`](docs/metrics.md). If a number surprises you, that document
  is the first place to look.

## Privileges

monitrs runs unprivileged and is designed to stay that way. Without elevated
privileges some metrics are unavailable — notably per-process I/O for processes
you do not own. Those appear as `permission denied`, not as zero, and the Inspect
screen lists exactly what is missing and why.

monitrs **never** escalates privileges on its own and never invokes `sudo`.

## Privacy

* No telemetry, of any kind, ever.
* No network access during normal operation. No update check.
* Snapshot export is explicit, excludes environment variable values, and redacts
  command arguments by default, because arguments frequently contain secrets.
* Logs are off by default. When enabled, command lines are redacted and
  environment values are never written.
* [`docs/platform-support.md`](docs/platform-support.md) documents every local
  file and OS interface monitrs reads.

## Development

```sh
just ci        # everything CI runs
just test      # cargo test --workspace --all-features
just clippy    # cargo clippy --workspace --all-targets --all-features -- -D warnings
just run       # cargo run -p monitrs
just snapshots # review pending insta snapshots
just bench     # criterion benchmarks
```

`just --list` shows every recipe with the underlying cargo command. `just` is a
convenience only; nothing requires it. See [`CONTRIBUTING.md`](CONTRIBUTING.md),
which also states what `1.0.0` froze and what it deliberately did not.

## Performance

Measured, with the machine and the command recorded in
[`docs/benchmarks.md`](docs/benchmarks.md). These are from a 12-core Mac running
about a thousand processes, which is five times §16.1's 200-process reference
workload, so read them as a hard case rather than a flattering one:

| Budget | Measured |
|---|---|
| frame render below 16 ms at 160×48 | median 200 µs, p95 353 µs |
| input-to-visible-response below 50 ms | median 417 µs, p95 486 µs |
| sample collection below 200 ms p95 | p95 12.63 ms for the ordinary fast-only tick (four in five), 40.90 ms when the medium tier joins it (every fifth), 134.78 ms for the every-thirtieth tick that also reads sensors |
| resident memory below 50 MiB | median 24.5–26.7 MiB, peak 27.2 MiB |
| no unbounded growth | 30-minute soak: resident size fell, descriptors flat, nothing dropped |
| idle self CPU below 1% median, 2% p95 | median 0.60–0.85% — met; **p95 4.30–9.50% — fails** |

The last row is the honest one, and where the cost sits is now measured rather than
guessed. monitrs' own computation is about 35 µs per tick; the rest is OS reads. The
process table and the disk counters used to cost 29 ms and 34 ms of it every second and
no longer do — both were fixed. What remains is the medium tier's filesystem-capacity
work, at **13.2–35.0 ms of CPU per tick** against a whole-tick budget of roughly 16 ms;
which of its two reads carries that is not yet separated. `docs/benchmarks.md` breaks it
down read by read and says what would close it. A twelve-hour soak has not been run, and
no soak has been run on Linux.

Two component results worth knowing: history seeking is constant time regardless of
how far back you scrub, and the sampling loop is bound by OS reads rather than by
anything monitrs computes.

## License

Dual-licensed under either

* [Apache License 2.0](LICENSE-APACHE), or
* [MIT license](LICENSE-MIT)

at your option.

Contributions are accepted under the same dual license.
