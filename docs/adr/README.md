# Architecture decision log

Each entry records a decision that was not forced, the alternatives that were
rejected, and what would make us revisit it. Entries are append-only: a
superseded decision gets a new entry rather than an edit, so the reasoning that
led to the old choice stays readable.

Status values: **accepted**, **superseded by NNNN**, **revisit**.

---

## 0001 — Four crates with a one-directional dependency graph

**Accepted.**

```
monitrs-core        no terminal, no OS collector
monitrs-collectors  -> monitrs-core
monitrs-tui         -> monitrs-core
monitrs (binary)    -> core + collectors + tui
```

A single crate would compile marginally faster and need no version coordination.
It was rejected because the boundary is what makes the interesting parts testable:
`monitrs-core` has no way to touch a real system, so rate arithmetic, history
seeking, tree construction, and diagnostic rules are all exercised from
fixtures. Likewise `monitrs-tui` cannot read `/proc`, so rendering is a function
of state.

The boundary is enforced by Cargo, not by convention. `monitrs-collectors` and
`monitrs-tui` do not depend on each other and cannot.

**Revisit if** the binary starts needing glue that belongs in neither collector
nor TUI, which would suggest a missing fifth crate rather than a merge.

---

## 0002 — Dual MIT OR Apache-2.0, replacing GPL-3.0

**Accepted.** Supersedes the repository's original GPL-3.0 `LICENSE`.

The repository began under GPL-3.0. It is now dual MIT/Apache-2.0, which is the
Rust ecosystem norm, is what the dependency license policy in `deny.toml`
presumes, and is what lets the crate be vendored into other tooling.

The change was made deliberately and with the repository owner's decision, before
any third-party contribution existed — which is the only point at which
relicensing is straightforward. `LICENSE-MIT` and `LICENSE-APACHE` replace the
single `LICENSE` file.

Note the asymmetry this creates: monitrs may not vendor GPL code, and `deny.toml`
allows no copyleft license.

---

## 0003 — `MetricState<T>` with a typed reason, plus a `Stale` variant

**Accepted.**

The specification sketches `TemporarilyUnavailable { reason: String }`. A `String`
in that position allocates in every field of every sample — thousands of
allocations per second at the default interval — so the reason is a small
`Copy` enum (`UnavailableReason`) and the human-readable text is produced at the
UI layer by `.message()`.

A sixth variant, `Stale { value, age }`, was added beyond the five the
specification lists. The specification permits retaining a last-good value across
a transient failure *only* if it is visibly marked stale and carries its age.
Encoding the value and the age together makes it impossible to render a retained
value without knowing it is stale — the rule becomes a type error rather than a
review comment.

Two consequences worth stating: `.fresh()` deliberately returns `None` for a
stale value, so staleness cannot leak into a calculation or a rate baseline; and
`.into_stale()` is a no-op on anything not currently `Available`, so staleness
cannot compound into a fabricated value.

---

## 0004 — `crossbeam-channel` over `flume`

**Accepted.**

Both provide bounded MPMC channels with select. `crossbeam-channel` was chosen
for `select!` maturity and because it is already ubiquitous in the ecosystem, so
it is unlikely to appear twice in the graph under different versions. `flume` is a
fine alternative and the choice is close; the point of recording it is that the
workspace must contain exactly one of them.

---

## 0005 — `color-eyre` over `anyhow` in the binary

**Accepted.**

`anyhow` is lighter. `color-eyre` was chosen because it installs a panic and error
hook, which is exactly the seam where terminal restoration has to happen: a panic
must restore the terminal *before* the report is printed, or the report lands on a
terminal still in raw mode and alternate screen.

Libraries use `thiserror` and typed errors; only the binary uses `color-eyre`.

---

## 0006 — No async runtime

**Accepted.**

Four threads (UI/reducer/render, terminal input, sampler, on-demand detail
worker) and bounded channels cover every concurrency need here. An async runtime
would be added purely for timers, which is not a reason.

This is enforced rather than documented: `deny.toml` bans `tokio` and
`async-std` outright, so a transitive dependency cannot pull one in unnoticed.

**Revisit if** a required platform API is only available async, which is not the
case for `/proc`, `sysctl`, or `crossterm`.

---

## 0007 — MSRV 1.95, and how `rust-toolchain.toml` interacts with the MSRV job

**Accepted.**

`rust-version = "1.95"` is the highest floor in the resolved graph, set by
`sysinfo` 0.39. It is a claim about the whole graph, so it is verified by a CI job
rather than asserted.

The subtlety: `rust-toolchain.toml` pins `stable` for development, and it
*overrides* `cargo +1.95.0`. An MSRV job written the obvious way would silently
test stable and pass. The `RUSTUP_TOOLCHAIN` environment variable takes precedence
over `rust-toolchain.toml`, so the MSRV job sets that instead. This is recorded
because it is a trap that produces a green, meaningless job.

---

## 0008 — `sysinfo` as the baseline, native code only where it is insufficient

**Accepted.**

`sysinfo` supplies CPU, memory, processes, disks, networks, components, and system
identity on both target platforms. Writing all of that natively twice would be a
large amount of platform code for no user-visible gain.

Native enrichment is added only where the baseline cannot express something the
product needs: Linux PSI, cgroup v2 limits, `/proc/diskstats` device busy time,
`/proc/<pid>/io`, and the macOS equivalents. These live behind the `linux-native`
and `macos-native` features, both on by default and both no-ops off their
platform.

Two operational rules follow: collector instances are long-lived, because several
metrics need a previous measurement and recreating the collector each tick both
wastes allocations and destroys deltas; and only the requested data groups are
refreshed, never an all-fields refresh.

---

## 0009 — A deliberately narrow clippy policy

**Accepted.**

`clippy::all` plus a short list of correctness lints, denied as errors in CI.
Notably **excluded**:

* `cast_precision_loss` — fires on every `u64 as f64` in a percentage
  calculation, which is the intended operation. Keeping it would produce dozens
  of `allow` attributes and train contributors to add them reflexively.
* `indexing_slicing` — fires on ordinary slice windows in the layout and
  formatting code.
* The `pedantic` group as a whole.

Notably **included**: `undocumented_unsafe_blocks` is `deny`, so the `SAFETY:`
comment requirement is mechanical rather than a review habit; `unwrap_used` and
`expect_used` warn in production code, because a panic in a TUI leaves the user's
terminal unusable. Both are allowed under `#[cfg(test)]` only, via a crate-root
`cfg_attr` — the narrowest scope that keeps tests readable.

---

## 0010 — `sysinfo`'s `multithread` feature is off

**Accepted, revisit.**

`sysinfo` can parallelise process refresh with `rayon`. It is off, because the
specification forbids optimizing on intuition and `rayon` is a substantial
dependency to add before any profile shows process refresh is the bottleneck.

**Revisit when** benchmarks at the 10,000-process reference load show refresh
dominating the fast tier. That is a measurement, not a guess, and the benchmark
to make it exists.

---

## 0011 — `panic = "unwind"` in the release profile

**Accepted.**

`panic = "abort"` would shrink the binary and speed up compilation. It is rejected
because terminal restoration runs in the panic hook and during unwinding. Aborting
would leave the terminal in raw mode with the alternate screen active — the exact
failure the specification calls out as unacceptable.

`strip = "symbols"` is kept for size, with the trade-off noted: crash backtraces
from a release binary are less useful. This is a conscious choice rather than a
default.

---

## 0012 — Process CPU is core-normalized by default

**Accepted.**

One core = 100%, so a process using four cores reads `400%`. This matches `top`
and `htop`, so existing intuition transfers, and it preserves information that
machine normalization discards: `400%` and `50% of 8 cores` are the same
measurement, but only the former makes the thread count visible.

`Percent` is therefore deliberately **not** clamped to 100 at construction.
Meters call `.clamped_to_100()` explicitly.

`process_cpu_normalization = "machine"` switches convention, and the active
convention is stated in help and in `docs/metrics.md` rather than left implicit.

---

## 0013 — Truncation operates on `char` boundaries, not grapheme clusters

**Accepted.**

Display-width-aware truncation uses `unicode-width` and iterates `char`s. Correct
grapheme handling would need `unicode-segmentation`, a second Unicode table in
the binary.

The trade-off is bounded and worth naming: a combining mark can be separated from
its base character in pathological input. What is *not* at risk is the width
budget — it is never exceeded, so a wide process name can never overflow its
column and corrupt the table, which was the actual failure mode being defended
against.

**Revisit if** a real-world process name renders visibly wrongly rather than
merely imperfectly.
