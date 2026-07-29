# monitrs

**A fast, keyboard-first system cockpit for Linux and macOS, built in Rust.**

monitrs shows you what your machine is doing right now — and, unlike most
terminal monitors, what it was doing thirty seconds ago. Pause the timeline,
scrub back to a spike, and see which processes were most strongly correlated
with it.

> **Status: pre-release.** This is a `0.1.0` in progress. Sections marked
> _pending_ are not yet true, and this README will not claim otherwise. See
> [`CHANGELOG.md`](CHANGELOG.md) for what actually works today.

## Demo

_Pending._ A real recorded demo and real screenshots land with `0.1.0`. This
project does not ship mocked-up screenshots: everything shown here will be
captured from the running binary.

The layout it is built towards, in strict ASCII mode:

```text
+ monitrs host:dev-mbp  LIVE  1.0s  up 3d 04:12 -------- 22:14:44 ----------+
| CPU  37% [#############----------------------]  load 4.12 3.84 3.21       |
| MEM  71% [#########################----------]  22.8/32.0 GiB  swap 0.2G  |
+ PRESSURE --------------------+-- HISTORY 5m -------------------------------+
| . CPU normal     37%         | CPU  .....::-=+*##@%#*+=--:...              |
| ! MEM watch      71%         | MEM  ====+++++*********########             |
| . DISK normal    12%         | I/O  .......:==#@@*=:........               |
| ? NET unknown   18M/s        |        ^ -00:37 selected                    |
+ PROCESSES --------------------------------------------------- 218 total ---+
|   PID USER       CPU%  MEM%     RSS   READ/s WRITE/s  AGE      COMMAND     |
|>31842 gabor      287%   8.1%   2.6G    18M     42M    00:43    rustc       |
|  1221 postgres    54%   3.0%   982M   128K      7M    12d      postgres    |
+----------------------------------------------------------------------------+
```

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
| Explicit per-metric availability | yes | partial | partial | partial |
| Shows the rule behind a warning | yes | n/a | n/a | n/a |
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
platform provides, and the Inspect screen (`5`) for what your specific machine
provides right now.

## Install

_Pending for `0.1.0`._ Release archives and SHA-256 checksums are produced by the
release workflow; install instructions will be added once they have been verified
on the target systems rather than merely written down.

Building from a checkout works today:

```sh
cargo build --release
./target/release/monitrs
```

## Keys

Full, always-current help is generated from the live keymap: press `?`.

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit |
| `?` | Context-aware help |
| `1`–`5` | Overview / Processes / Storage / Network / Inspect |
| `Tab`, `Shift-Tab` | Next / previous panel |
| `Space` | Pause or resume the visible timeline |
| `[` `]` | Step back / forward through history |
| `L` | Return to live |
| `/` | Filter |
| `n` `N` | Next / previous match |
| `s` `S` | Sort selector / reverse sort |
| `f` | Toggle flat and tree view |
| `p` | Pin or unpin the selected process |
| `Enter` | Inspect the selected item |
| `x` | Signal dialog for the selected process |
| `:` | Command palette |
| `t` `g` | Cycle theme / glyph mode |
| `r` | Force refresh |
| `Esc` | Close overlay or cancel |

`T`, `K`, and `R` *propose* SIGTERM, SIGKILL, and renice. None of them acts on a
single keypress; each opens a confirmation showing the process identity and the
consequences.

## Configuration

monitrs is useful with no configuration and **does not create a config file on
first launch**.

```sh
monitrs config path     # where it looks
monitrs config init     # write a documented starter file (never overwrites)
monitrs config check    # validate without launching
```

CLI flags override file values. See
[`docs/configuration.md`](docs/configuration.md) for every key.

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
convenience only; nothing requires it. See
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## Performance

Engineering budgets are recorded in
[`docs/architecture.md`](docs/architecture.md); they are **budgets, not
measurements**.

Component benchmarks that *have* been measured — with the reference machine and
the exact command — are in [`docs/benchmarks.md`](docs/benchmarks.md). Two results
worth knowing: history seeking is constant time regardless of how far back you
scrub, and at the reference workload the sampling loop is bound by OS reads rather
than by anything monitrs computes.

The end-to-end budgets (idle CPU, resident memory, input latency, frame time) are
not measured yet and this README will not publish a number until they are.

## License

Dual-licensed under either

* [Apache License 2.0](LICENSE-APACHE), or
* [MIT license](LICENSE-MIT)

at your option.

Contributions are accepted under the same dual license.
