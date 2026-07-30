# monitrs

**A fast, keyboard-first system cockpit for Linux and macOS, built in Rust.**

monitrs shows you what your machine is doing right now — and, unlike most
terminal monitors, what it was doing thirty seconds ago. Pause the timeline,
scrub back to a spike, and see which processes were most strongly correlated
with it.

> **Status: pre-release.** `0.1.0` is written and its release notes are in
> [`CHANGELOG.md`](CHANGELOG.md), but it has not been tagged, so there is nothing to
> download yet. Everything this README describes works from a checkout; where a
> claim has a caveat, the caveat is stated next to it rather than left out.

## Demo

A real frame, captured from the running program on a Mac with about a thousand
processes, in strict ASCII mode with colour off — the form that survives a README, a
terminal without colour, and a screen reader. It is written by
`crates/monitrs/tests/capture.rs` straight out of the renderer with live data
(`cargo test -p monitrs --release --test capture -- --ignored`), so it cannot drift
from what monitrs actually draws, and §20.1's ban on a mocked-up screenshot is kept.

Trimmed to 118 columns on the right for legibility; the full 160-column frames, the
Unicode variant, and the 80×24 compact layout are in
[`docs/screenshots/`](docs/screenshots/). The hostname and the login name are
substituted for the machine this was taken on — every measurement, process name and
state is exactly as rendered, and the substitutes are the same width so no column
moves.

```text
+ monitrs  host:dev-mbp  [>LIVE]  250ms  up 9d 03:41 -----------------------------------------------------------------
| CPU   49% [##############################################=-----------------------------------------------]  load  7.
| MEM   67% [###############################################################=------------------------------]  32G/48G
+ PRESSURE -----------------------+ HISTORY 5m -----------------------------------------------------------------------
| . CPU     normal        49%     | CPU
| . MEM     normal        33%     | MEM
| . NET     normal     3.4K/s     | I/O
| . LOAD    normal       7.03     | CORE  +=--+*******
|                                 |
+ PROCESSES  sort CPU% desc  no kthreads -----------------------------------------------------------------------------
|      PID USER     S   CPU%  MEM%   RSS  VIRT  READ/s WRITE/s  THR      AGE NAME                             COMMAND
|>   55629 me       R   263%  1.0%  473M  416G    0B/s  8.3M/s    7    00:09 rustc                            /Users/y
|    45241 me       R    14%  1.5%  738M  1.8T    0B/s    0B/s   26 03:59:44 Cursor Helper (Renderer)         /Applica
|    45234 me       R    12%  0.2%  103M  464G    0B/s    0B/s   19 03:59:45 Cursor Helper                    /Applica
|     1398 me       R    10%  0.3%  129M  416G    0B/s    0B/s   10       9d Terminal                         /System/
|    55806 me       R   8.9%  0.1%   26M  415G    0B/s    0B/s    5    00:07 capture-35a93566c2c11cd4         /Users/y
|    37194 me       R   7.6%  0.5%  245M  1.8T    0B/s    0B/s   22    08:47 Google Chrome Helper (Renderer)  /Applica
|    30365 me       R   7.4%  0.1%   32M  415G    0B/s    0B/s    9    12:39 probe_input_latency-fbafe9f20... /Users/y
|    12764 me       R   5.8%  0.9%  453M  1.8T    0B/s    0B/s   44    26:24 Google Chrome Helper (Renderer)  /Applica
|    62728 me       R   5.8%  1.6%  788M  421G    0B/s    0B/s   34 14:21:38 claude                           claude
|    11623 me       R   3.8%  0.8%  370M  1.8T    0B/s    0B/s   25    27:38 Google Chrome Helper (Renderer)  /Applica
|    84339 me       R   2.0%  0.7%  328M  465G    0B/s    0B/s   21       3d Google Chrome Helper             /Applica
|     4571 me       R   1.7%  0.1%   25M  415G    0B/s    0B/s    4       2d python                           /Users/y
|    12800 me       R   1.6%  0.0%   24M  415G    0B/s    0B/s    4       3d python                           /Users/y
|    55337 me       R   1.5%  0.4%  199M  1.8T    0B/s    0B/s   22 03:37:45 Cursor Helper (Plugin)           Cursor H
|    10421 me       R   1.4%  0.8%  415M  421G   38K/s    0B/s   29 18:55:36 claude                           claude
```

Things worth noticing, all of which are the design rather than the accident of one
frame:

* **`>` marks the selection, and it starts on the busiest process** — not on PID 1.
  The cursor follows a process once you choose one, and until then it tracks the top
  of the table (§7.2).
* **`normal` is spelled out next to a `.` symbol.** Colour is never the only carrier
  of meaning, so every state has a redundant character (§5.2).
* **`n/a` and `denied` are different words.** A metric the OS refused to report is
  not the same as one this machine cannot produce, and neither is a zero (§4, §26).
* **`THR` has real numbers.** They come from the native macOS layer; the
  cross-platform baseline cannot see them, which is why monitrs enriches by default
  (§9.2).

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

### From a checkout

Works today, and is what every measurement in this README was taken from:

```sh
cargo build --release
./target/release/monitrs
```

`q` quits. See [`docs/troubleshooting.md`](docs/troubleshooting.md) if a number
surprises you.

### From a release archive

_No release is published yet_ — `0.1.0` has not been tagged. When it is, the
workflow builds one archive per target (`x86_64` and `aarch64` for Linux glibc,
Linux musl, and macOS), each carrying the binary, both licences, this README, an
excerpt of the changelog for that version, shell completions for bash, zsh, fish,
PowerShell and elvish, and a manpage. Installing one means verifying it and putting
the binary somewhere on your `PATH`:

```sh
# Replace the version and target with the archive you downloaded.
shasum -a 256 -c monitrs-0.1.0-aarch64-apple-darwin.tar.gz.sha256
tar xzf monitrs-0.1.0-aarch64-apple-darwin.tar.gz
install -m 755 monitrs-0.1.0-aarch64-apple-darwin/monitrs ~/.local/bin/monitrs
```

Releases also carry a build attestation, so `gh attestation verify
monitrs-*.tar.gz --repo gaborini/monitrs` confirms the archive came from this
repository's workflow rather than from someone else.

Honest caveat: the assembly and these steps have been carried out by hand for
`aarch64-apple-darwin` only — checksum verified, binary run, manpage rendered,
completions parsed. The other five archives are built by CI and nobody has run them
on their own hardware yet.

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
consequences, and the forceful ones want a distinct key rather than `Enter`, so
leaning on the confirm key cannot escalate. The identity is rechecked immediately
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

Measured, with the machine and the command recorded in
[`docs/benchmarks.md`](docs/benchmarks.md). These are from a 12-core Mac running
about a thousand processes, which is five times §16.1's 200-process reference
workload, so read them as a hard case rather than a flattering one:

| Budget | Measured |
|---|---|
| frame render below 16 ms at 160×48 | median 200 µs, p95 353 µs |
| input-to-visible-response below 50 ms | median 417 µs, p95 486 µs |
| sample collection below 200 ms p95 | p95 59–71 ms for the ordinary tick; 136–149 ms for the every-fifth one |
| resident memory below 50 MiB | median 29 MiB, peak 31 MiB |
| no unbounded growth | 30-minute soak: resident size fell, descriptors flat, nothing dropped |
| **idle self CPU below 1% median, 2% p95** | **median 1.3–2.7%, p95 11–15% — fails** |

The last row is the honest one. monitrs' own computation is about 35 µs per tick;
the cost is OS reads, and on this host the process table and the disk counters cost
29 ms and 34 ms respectively every second. `docs/benchmarks.md` breaks it down read
by read and says what would close it. A twelve-hour soak has not been run, and no
soak has been run on Linux.

Two component results worth knowing: history seeking is constant time regardless of
how far back you scrub, and the sampling loop is bound by OS reads rather than by
anything monitrs computes.

## License

Dual-licensed under either

* [Apache License 2.0](LICENSE-APACHE), or
* [MIT license](LICENSE-MIT)

at your option.

Contributions are accepted under the same dual license.
