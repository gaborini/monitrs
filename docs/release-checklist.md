# Release checklist

Everything needed to cut a monitrs release, in order, written for someone who has
never done it. Follow it top to bottom. Nothing here is optional except where it
says so.

## Read this first: what this project still owes

**The interface launches**, and the §23 verbs — launch, monitor, filter, inspect,
pause, seek, return live, quit — have each been exercised by hand in a real
terminal. So has the renice flow, by `scripts/verify-renice.py`: it drives the real
binary on a pty, presses `R` on a process it started itself, and then asks `ps` —
not monitrs — whether the value changed.

`0.1.0` and `0.2.0` are both published, as pre-releases, with the items below still
open. That was a deliberate call each time rather than an oversight, and the point of
this section is that the open items travel with the release instead of being forgotten:
they are repeated in each `CHANGELOG.md` release section under *Known limitations*, which
is what a user actually reads. What is still owed:

* **One of the four freeze guards does not block yet, and it is the API one.**
  `.github/workflows/ci.yml`'s `semver compatibility` job runs
  `continue-on-error: true`, so it reports a break and CI stays green. Flipping it to
  `false` is a step of the `1.0.0` release commit ([step 2](#2-bump-the-version)) and
  is the only thing that makes the *API* half of CONTRIBUTING.md's freeze enforceable
  rather than aspirational. The other three surfaces — the JSON export, the
  configuration keys and the default keymap — are each guarded by a test that already
  fails a build ([step 4](#and-the-four-freeze-guards)). This one is called out here
  as well as there because nothing fails if it is skipped.
* **No twelve-hour soak is on record**, and it is not going to be produced from a
  workstation: the gate is twelve uninterrupted hours, and a laptop that sleeps does not
  yield one. It is being moved to a dedicated EC2 host under its own project. A 30-minute
  run with the shipped collector
  is ([`soak-testing.md`](soak-testing.md#runs-on-record)): resident size fell over
  the run, descriptors stayed at 3, retained history stayed bounded with the ring
  full, and nothing was dropped even with the channel saturated. That is evidence,
  not the gate — §16.1 says twelve hours. (The 90 ms worst-case input latency that run
  reported turned out to be the harness measuring two thread wake-ups rather than
  monitrs; see [`soak-testing.md`](soak-testing.md).) For `1.0.0` specifically, that
  gate was **deferred past the tag by a written decision** rather than met or skipped:
  [step 5](#5-soak) names who decided, on what date, on what evidence, what the
  deferral does not cover, and the seven days inside which the runs are owed.

  **The debt was paid on 2026-08-02, inside the window, and the gate is met.** Twelve
  hours on both Tier 1 Linux architectures: resident memory 14 490 → 15 083 KiB on
  x86_64 and 16 097 → 16 212 KiB on aarch64, first quartile to last, against a 16 384 KiB
  allowance, with the history ring at one size across all 720 measurements and
  descriptors at 4. The first attempt, on 2026-08-01, failed — in this repository's own
  test harness rather than in monitrs, and by enough that the gate could not have passed
  at any input rate. If you are about to run this for a release, read
  [`soak-testing.md`](soak-testing.md) first: two of the traps in that document were only
  found by running it for real.
* **The idle self-CPU budget of §16.1 is not met, and on its own reference workload
  neither half of it is.** On 8-vCPU Linux at 199–200 processes — the workload the budget
  names — median **2.66%** against 1% and p95 **3.99%** against 2%, on both Tier 1
  architectures, twelve runs agreeing. On a 12-core Mac with about a thousand processes
  the median passes at **0.60–0.85%** while the p95 fails at **4.30–9.50%**, which is
  where the older "half met" reading came from; fewer processes did not make it cheaper.
  Note also that the reference-workload readings are quantised in 1.33% steps, so a
  passing median is only reportable as 0.00%. The remaining cost is measured rather
  than suspected: the medium tier's two filesystem-capacity reads cost **13.2–35.0 ms of
  CPU per tick**, positive in 15 of 15 runs, against a whole-tick budget of roughly 16 ms.
  Which of the two carries it has not been separated. (This list previously quoted
  1.3–2.7% and 11–15%, and then 0.5–1.1% and 6–11%; both were superseded in
  [`benchmarks.md`](benchmarks.md) before this list was next read.)
  Four of the other five measurable budgets pass outright — frame render, input latency,
  collection p95, resident memory — and as of 2026-08-02 so does the twelve-hour run,
  memory and descriptors both, on two Linux architectures.
  [`benchmarks.md`](benchmarks.md#the-161-end-to-end-budgets) has the numbers, the
  per-read breakdown showing where the time goes, and what it would take to fix.
  This is the one item on this list that is a *product* problem rather than a
  procedural one. Every figure quoted for it, on this machine and in every prior
  release before 1.0.1, is against about a thousand processes — five times §16.1's own
  200-process, 8-CPU reference workload. That reading was taken on 2026-08-02 and is
  quoted above; it is worse, not better. Step 6 below has it as a step, and
  [`benchmarks.md`](benchmarks.md#reading-the-idle-cpu-budget-on-its-own-reference-workload)
  has the protocol.
* **A release section's date is the day you tag, not the day you wrote it.** Keep a
  Changelog dates a section by its release date, so if the notes were written yesterday
  and the tag goes out today, change that line first. The workflow's `verify` job passes
  `--check-date`, which *warns* on a heading that is not dated today rather than failing —
  a hard check there would only teach people to backdate the heading.

  Done for `v0.1.0`: six archives and a `SHA256SUMS` are on the release page, every
  checksum verifies against the *downloaded* files, and every archive carries a build
  attestation that `gh attestation verify` accepts. The `aarch64-apple-darwin` archive
  was extracted and run from the published tarball — eleven files, `monitrs 0.1.0`, a
  996-process snapshot. All four crates are on crates.io and `cargo install
  monitrs --locked` works from the registry.

  Before the tag, **both macOS archives** were also assembled and exercised by hand,
  following the workflow's own steps: each tarball holds the binary, both licences, the README, the
  changelog excerpt, five shell completions and the manpage — eleven files; both
  checksums verify; both extracted binaries report `monitrs 0.1.0` and produce a
  snapshot of 1027 processes; both manpages render under `man`; both sets of bash and
  zsh completions parse under `bash -n` and `zsh -n`.

  That includes `x86_64-apple-darwin`, which **CI builds but cannot run** — it skips
  the smoke test for that target because GitHub's Intel runners are being retired. Run
  under Rosetta here it works, with one honest difference: temperatures come back
  `unsupported` where the arm64 build says available (see
  [`platform-support.md`](platform-support.md)). The four Linux archives cannot be
  built on this machine — no cross-linker — so they remain CI's to produce and
  nobody's to have run.

So this file is a **procedure, not a plan**: it is complete and it is followed as
written. The [§23 gate](#the-23-gate) at the bottom is the record of what has been
proven, and it still has unticked boxes — the idle-CPU p95 and the soak. Two releases
have gone out over those boxes, deliberately and with the pre-release flag the workflow
applies to every `0.x`, and each one repeats the open items in its own changelog section
under *Known limitations*. `1.0.0` ships over the soak box as well, and it is the first
release to do so **without** that flag, which is why [step 5](#5-soak) carries a dated,
attributed decision with a deadline instead of one more repetition of this paragraph.
That is the arrangement: a box may be shipped over, but not quietly, and never by
editing the box.

## 0. Prerequisites

On the machine you are releasing from:

```sh
rustc --version          # must be stable and at least the MSRV in Cargo.toml
cargo --version
python3 --version        # the release workflow's helper scripts are python3
gh --version             # for verifying the published artifacts
```

You also need:

* push access to the repository, including tags;
* a crates.io token, for step 10 only, and **only for as long as that step takes**.
  §18.4 still says no publishing token belongs in this repository or in a workflow: the
  release workflow uses the run's own `GITHUB_TOKEN` and never publishes crates. Obtain
  the token, `cargo login` with it, publish, then `cargo logout`. Do not paste it into a
  terminal that keeps scrollback, a chat window, or a shell history file — a token that
  has been seen anywhere else is a token to revoke.

Since `0.1.0`, crates.io publication **is** part of a release: all four crates are
published, and the README tells users to `cargo install monitrs --locked`. Step 10 is
where that happens; it is deliberately the last step, after the tag is proven.

Optional: `just`, a thin wrapper around the cargo commands below. Every recipe
prints the command it runs, and this checklist gives the underlying commands so
`just` is never required.

## 1. Decide the version

* SemVer (§19.1). Pre-`1.0`, a breaking change bumps the minor.
* Tags are `vX.Y.Z`. The release workflow triggers on `v[0-9]+.[0-9]+.[0-9]+*`, so
  `v0.1.0-rc.1` also triggers it.
* Anything `0.x` is published as a **pre-release** automatically, so nobody mistakes
  it for a stability promise.
* §19.1's rule was **do not publish to crates.io until the name and metadata are
  final**. They are: the four crate names are taken and the metadata shipped with
  `0.1.0`, so every release from now on publishes (step 10). Renaming a published crate
  is not possible, which is what that rule was protecting.

## 2. Bump the version

> **`1.0.0` only, and do it in this same commit: make the semver job blocking.**
> `.github/workflows/ci.yml`'s `semver compatibility` job runs with
> `continue-on-error: true`, because before `1.0.0` there is no published 1.x
> release to be compatible against and a failure would be noise. Set it to
> `false` and delete the comment above it that says to.
>
> This is the **only blocking mechanism the API freeze has**. Everything
> CONTRIBUTING.md's "What 1.0.0 froze" section promises about the public API is,
> until this flip, a policy that nothing enforces — the job reports and CI stays
> green. It is also the single most forgettable step in the release, because
> nothing fails if you skip it: CI passes, the tag ships, and the freeze is
> unenforced until somebody notices. Do it in the release commit at step 7, where
> the reviewer of that pull request can see it.

The version lives in **exactly five places**, and three of them are one file. Every
crate uses `version.workspace = true`, so no per-crate manifest is touched.

1. `Cargo.toml`, `[workspace.package]`:

   ```toml
   version = "X.Y.Z"
   ```

2. `Cargo.toml`, `[workspace.dependencies]` — the three internal crates carry a
   version requirement beside their path, and it must move in lockstep or the
   workspace stops resolving:

   ```toml
   monitrs-core = { version = "X.Y.Z", path = "crates/monitrs-core" }
   monitrs-collectors = { version = "X.Y.Z", path = "crates/monitrs-collectors" }
   monitrs-tui = { version = "X.Y.Z", path = "crates/monitrs-tui" }
   ```

3. `Cargo.lock` — four entries, one per workspace crate. **Do not hand-edit it.**
   Regenerate:

   ```sh
   cargo check --workspace
   ```

   The lock file is committed because this repository ships an application (§13),
   and the release builds with `--locked`, so a stale lock fails the release rather
   than silently building something else.

4. `CHANGELOG.md` — the next step.

5. `README.md` — the status banner, the release link, and the three hard-coded archive
   filenames in the install snippet. This one is easy to miss and expensive to miss:
   `.github/workflows/release.yml` copies `README.md` into **every** archive, and
   `crates/monitrs/Cargo.toml` sets `readme = "../../README.md"`, so a stale banner
   becomes the crates.io landing page for the new version. Nothing in CI checks it.
   Include it in the release commit at step 7 so the tagged commit the archives are
   built from already carries it.

Confirm what the tooling now sees. This is the exact command the release workflow
compares against your tag:

```sh
cargo metadata --format-version 1 --no-deps \
  | python3 .github/scripts/package_version.py monitrs
```

It must print `X.Y.Z` and nothing else.

## 3. Update the changelog

`CHANGELOG.md` follows Keep a Changelog, and its own preamble sets the rule that
matters: *entries describe what a user can observe.*

1. Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` (ISO date, the day you
   expect to publish).
2. Add a fresh, empty `## [Unreleased]` above it.
3. Update the link definitions at the bottom of the file.
4. Re-read every entry against the binary you are about to ship. Delete anything a
   user cannot observe. If the interactive interface is still absent, the changelog
   must say so in the release section, not only in this file.

Verify the release workflow can find the section. This is the same script the
workflow runs, and it exits non-zero when there is no section — which is how a
release with empty notes is prevented:

```sh
python3 .github/scripts/changelog_excerpt.py X.Y.Z
```

It fails for a version with no section, which is how a release with empty notes is
prevented. Try it with a version you have not written yet:

```text
$ python3 .github/scripts/changelog_excerpt.py 9.9.9
CHANGELOG.md has no section for 9.9.9
$ echo $?
1
```

## 4. Run every gate locally

Same commands, same order as CI. Do not skip ahead on a failure.

```sh
python3 .github/scripts/check_workflows.py
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
```

Then the tests CI runs separately, because they are `#[ignore]`d:

```sh
cargo test --workspace --all-features -- --ignored --test-threads=1
```

That pass includes the platform smoke tests **and** the soak harness at its default
ten seconds. Ten seconds is not the soak gate; step 5 is.

**It also rewrites tracked files.** `crates/monitrs/tests/capture.rs` writes a fresh frame
for every screen into `docs/screenshots/`, so a clean tree is dirty afterwards, with seven
files changed. That is the point — §20.1 allows no screenshot that is not written from the
renderer — but decide deliberately whether the new frames belong in the release:

```sh
git status --porcelain docs/screenshots/
git diff docs/screenshots/          # a captured frame from your machine, not a diff to review line by line
```

Either commit them with the release or `git restore docs/screenshots/`. Do not leave them
uncommitted: the next `git add -A` picks them up in whatever commit comes along.

Also confirm the dependency graph has not drifted:

```sh
# The real check, and the one CI gates on: it must print exactly one version.
cargo metadata --format-version 1 --all-features \
  | python3 .github/scripts/crate_versions.py crossterm

# For reading the graph by eye. `-p` cannot select crossterm — it is a dependency, not a
# workspace member — so this exits non-zero and CI wraps it in `|| true`. Informational.
cargo tree --workspace --all-features -i crossterm
```

### And the platform you are not on

Everything above compiles **one** of the two native layers. On a Mac,
`crates/monitrs-collectors/src/linux/` is behind `cfg(target_os = "linux")` and is never
seen — not by `cargo check`, not by `clippy`, and not by `rustdoc`. Half the collector can
therefore be broken while every gate above passes.

```sh
rustup target add x86_64-unknown-linux-gnu      # once; aarch64-apple-darwin from Linux
cargo check --workspace --all-features --target x86_64-unknown-linux-gnu
cargo clippy -p monitrs-collectors --all-features --lib --tests \
  --target x86_64-unknown-linux-gnu -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features \
  --target x86_64-unknown-linux-gnu
```

None of these links, so the other platform's toolchain is not needed — only its `std`.

The third command is the one that is easy to leave out and the one that has already caught
something: a push went red on `cargo doc` alone, because a public module doc in
`linux/statfs.rs` linked to a private constant. `clippy --target` does not run `rustdoc`,
so the first two commands passed while the release gate would not have. Run all three.

`--all-features` matters here too: the native layers are behind `linux-native` and
`macos-native`, so omitting it checks the `sysinfo` baseline and nothing else.

### And the four freeze guards

`1.0.0` froze four surfaces and gave each one a guard. Three of them already ran in
the `cargo test` above; the fourth runs locally only if you run it yourself.

| Frozen surface | Guard | Where it runs |
|---|---|---|
| the public API of `monitrs-core`, `monitrs-collectors`, `monitrs-tui` | `cargo-semver-checks` | the `semver compatibility` CI job — and locally, below |
| the JSON export | `docs/schema/v2.json` (292 field paths) + `crates/monitrs/tests/schema_contract.rs` | `cargo test` |
| the configuration keys | `docs/schema/config-v1.json` (32 key paths) + `crates/monitrs/tests/config_contract.rs` | `cargo test` |
| the default keymap | `every_global_binding_from_the_spec_resolves_in_normal_mode` and its list-binding sibling, `crates/monitrs-tui/src/keymap.rs` | `cargo test` |

Run the semver check by hand before you tag. CI's copy stays advisory until the
`1.0.0` release commit flips it (step 2), and a job that reports without blocking
looks exactly like a job that passed:

```sh
cargo install cargo-semver-checks --locked      # once
cargo semver-checks check-release --workspace
```

`monitrs` itself declares a `[[bin]]` and no `[lib]`, so `--workspace` checks exactly
the three crates CONTRIBUTING.md's freeze names as API.

**Then look at what the two inventory tests printed**, which the run above hid.
Removing a path fails the test and says so loudly; *adding* one is deliberately only
printed, because a consumer reading by name cannot be broken by a field it has never
heard of. `cargo test` captures the stdout of a passing test, so the additions are
invisible unless you ask:

```sh
cargo test -p monitrs --test schema_contract --test config_contract -- --nocapture \
  | grep '^new '
```

Every line it prints is a field path or configuration key this release adds and the
inventory does not yet record. Add them to `docs/schema/v2.json` or
`docs/schema/config-v1.json` — deliberately, having read them, which is why the test
prints rather than regenerates. An unrecorded path is not a broken promise, but it is
a promise nobody wrote down, and the next release cannot notice it disappearing.

**A guard that fails is a decision, not an obstacle.** Adding a field, a key or a
binding is not a break. Removing or renaming one is, and each has its own price:
`SCHEMA_VERSION` bumped with a new `docs/schema/v{N+1}.json` written *beside* the old
file; `SUPPORTED_VERSION` bumped with a new `docs/schema/config-v{N+1}.json` beside
the old one, plus `docs/configuration.md`; a major version for the API; a major
version for a rebound default key. The old inventory file always stays, so a consumer
can diff the two. Each test's own failure message spells its rule out.

**And the one thing none of the four can see:** a frozen key or field whose *meaning*
changes while every name stays put. `sampling.slow_interval` taking over the sensor
read in `1.0.0` is the worked example — no inventory moved, and nothing failed. If a
release changes what a frozen name *does*, `CHANGELOG.md` and the relevant document
are the only record there will ever be, and writing them is part of the release rather
than a follow-up to it.

## 5. Soak

**Blocking, when the release can reach what it measures.** Moving the gate still costs
a written decision each time — that is what stops it becoming a habit — but the gate no
longer applies to every release regardless of content.

### When it applies

The soak protects three properties: no unbounded memory growth, no descriptor leak, no
stall under sustained load. Those live in a knowable set of places, so run the soak when
the diff since the last tag touches any of:

* `crates/monitrs-core/src/` — the data model, history ring and rate engine;
* `crates/monitrs-collectors/` — every per-tick allocation and every descriptor;
* `crates/monitrs-tui/src/app/` — the reducer, which owns every state transition;
* `crates/monitrs/src/runtime.rs` — the sampler, the bounded channel, coalescing;
* `crates/monitrs/src/interactive.rs` — the worker lifecycle and the frame loop;
* `crates/monitrs/tests/soak.rs` — the harness. On 2026-08-01 the harness *was* the leak;
* `Cargo.lock` — a dependency moved. Check whether it is on the runtime path.

And run it regardless of the diff for **every minor release, and at least quarterly**,
because dependency and platform drift arrive without a diff you would notice.

Don't run it for documentation, CI configuration, CLI or configuration parsing, export
formatting, the schema inventories, or packaging. A rule that demands fourteen hours and
ten dollars for a README typo gets quietly ignored, and a gate that is quietly ignored is
worse than a narrower one that is followed.

`.github/scripts/soak_required.py <last-tag> HEAD` answers this mechanically, and CI's
**soak required?** job prints the verdict into the run summary on every push. It reports
rather than blocks: nothing in CI can know whether you actually ran the soak. Its purpose
is that the question is answered once per release instead of argued each time.

> **`1.0.0` was the one exception, and it is settled.** The gate was **deliberately
> deferred until after the tag** by Gabor Lepsenyi (`@gaborini`) on **2026-08-01**, with a
> seven-day window. All three runs completed on **2026-08-02** and all three passed; the
> record is below. The first attempt failed, in this repository's own harness rather than
> in monitrs — which is why `tests/soak.rs` is on the list above.

[`soak-testing.md`](soak-testing.md) has the invocations, how to read the report, and
what to record. In short:

* [x] twelve-hour release-profile run, report kept — 2026-08-02, both architectures;
* [x] one-hour 10,000-process run, report kept — 2026-08-02, both architectures;
* [x] one-hour real-collector run **on Linux** — done 2026-08-02, both architectures; the only configuration in which the
      file-descriptor budget is actually exercised, because the macOS collector opens
      no files;
* [x] all three reports attached to the release record, with machine, toolchain, and
      profile — [`soak-testing.md`](soak-testing.md#2026-08-02--the-three-release-runs-101-and-they-pass).

### Why the deferral was defensible, and what it does not cover

**There is evidence, and it is not the gate.** One soak is on record
([`soak-testing.md`](soak-testing.md#runs-on-record), 2026-07-30): the shipped
collector, thirty minutes, `--release`, on an Apple M4 Pro under rustc 1.97.1 with
about a thousand other processes on the machine. Over 2212 snapshots resident size
*fell* — `first quartile 30880 KiB, last quartile 29192 KiB`, peak 31,184 KiB —
descriptors were "flat at 3", retained history did not rise — 2,009,802 B at the
start and 1,998,685 B at the end, against the ring's own 5,512,800 B worst case, with
the ring full at 300 of 300 samples so that is the steady state and not a ring still
filling — and nothing was dropped even when the stall
probe filled the channel to 64 of 64. That is a flat half hour where §16.1 asks for
twelve, and the difference is not rhetorical: it is enough to say the curve has the
right shape, not enough to say it holds.

**And what the deferral risks is cheap to fix forward.** If the twelve hours turn up
a leak or a descriptor climb, that is a behavioural defect, not an API break — none
of the four surfaces `1.0.0` froze can move because of it. The vehicle is a `1.0.1`
patch under the ordinary [Rollback](#rollback) procedure, and the freeze is
unaffected either way. That asymmetry is the whole argument for tagging first: the
promise this release actually makes is the one no soak result can touch.

**What the argument does not cover: Linux.** Nothing has been soaked on Linux at all,
and the descriptor budget is only meaningfully exercised there. The macOS collector
reads through `sysctl`, `libproc`, mach routines and `getifaddrs` and opens no file,
so `flat at 3` on macOS is flat by construction — it confirms the runtime holds
nothing open and says nothing about the collector that reads `/proc` constantly. So
of the three runs above, **the one-hour Linux real-collector run is the one carrying
real unknown**; the other two extend a measurement that already looks right on a
platform where it has been taken. If only one of the three can be run, run that one
first.

### The deadline

**All three runs and their records within seven days of the `v1.0.0` tag — by
2026-08-08.** Seven days because the three runs are fourteen hours of machine time on
a host [step 6](#6-verify-by-hand-on-real-machines) already provisions for the
reference-workload reading, which leaves the week as provisioning plus room for one
run to fail and be repeated — and because a defect found inside a week still reaches
roughly the people who installed `1.0.0`, where one found in a month reaches
strangers.

If a run fails, the report goes into
[`soak-testing.md`](soak-testing.md#runs-on-record) with the series that produced it,
the `v1.0.0` release gets the warning [Rollback](#rollback) step 2 describes, and the
fix ships as `1.0.1`. If the window passes with nothing run, **that is itself the
failure and is reported as one**: the next release section says the deferral went
overdue, naming this date, rather than restoring "the twelve-hour run is still owed"
to the list it came from. That sentence has been true since `0.1.0`; repeating it
once more would be exactly the quiet erosion this decision was written down to avoid.

## 6. Verify by hand, on real machines

Automation cannot do these. Do them on **both** a Linux machine and a macOS
machine, and write down the version and architecture of each.

The idle-CPU budget, on §16.1's own reference workload (8 logical CPUs, 200
processes, a one-second interval, a five-minute history) rather than on whatever
this repository's own development machine happens to be running. Every idle-CPU
figure in `benchmarks.md` up to this release is against roughly a thousand
processes — five times the reference — so this box is the first time the budget
is read on the workload it names.

* [ ] On an 8-vCPU Linux instance (Task 12 already has one, running the release
      archives), extract the tagged archive and run
      `MONITRS_BINARY=/path/to/extracted/monitrs python3 scripts/measure-overhead.py`.
      [`benchmarks.md`](benchmarks.md#reading-the-idle-cpu-budget-on-its-own-reference-workload)
      has the full protocol, the instance shape, and what each outcome means —
      decided in advance, not written after the fact.
* [ ] Confirm the script's own printed `workload:` line says the run matched the
      reference before trusting the median and p95 beside it; if it did not
      match, record it as the hard case it is rather than as a reading of the
      budget.
* [ ] Both figures — median and p95 — recorded in `benchmarks.md`, beside the
      existing ~1000-process numbers, and this list's own idle-CPU item above
      updated to say which workload each figure is.

**Two rows, not one**, every time this measurement is taken — on the reference host
and on whatever the development machine is. Since `1.0.0` the sensor group's cadence
follows what is on screen (§8.6), so idle self-CPU is two different numbers and
publishing one of them alone hides which:

* [ ] **Overview visible, untouched.** This is the row §16.1's budget is about, and
      the only one a pass or a fail is read from. Three runs of 60 s.
* [ ] **The Battery screen visible**, where the sensors return to `medium_interval`.
      Three runs of 60 s. This row is not a gate — it is the evidence that the
      cadence switch is doing something, and `benchmarks.md`'s two-row table is
      where it goes, with a sentence saying which of the two the budget names.
* [ ] Read the **medians** against each other and treat a p95 comparison between the
      two rows with suspicion: a p95 over ~74 samples is the fourth-largest value in
      the window, and the two rows' p95 ranges have already overlapped once across
      three runs each while their medians separated cleanly. `benchmarks.md`'s "Why
      there are two idle rows" explains the instrument; do not re-derive it.

Terminal behaviour. The first two are now **automated** —
`python3 scripts/verify-terminal-restoration.py` runs the release binary on a real pty,
stops it five ways, and checks both the escape sequences and the pty's `termios` state,
so run that rather than doing it by eye. Keeping them listed because a machine of your
own is still the thing being tested:

* [x] The binary launches and quits with `q`, leaving the terminal usable: cursor
      visible, echo on, no alternate screen, `stty sane` not needed.
* [x] The same after `Ctrl-C`, after `SIGTERM`, after `SIGHUP`, and after a deliberate
      panic. §14.3 requires restoration to survive partial initialisation and a panic.
      A panic could not be provoked through the interface, so that one is covered by
      unit tests rather than by the script; `SIGTERM` and `SIGHUP` needed a fix before
      they passed.
* [ ] The same after resizing to below the minimum and back, and after
      `SIGSTOP`/`SIGCONT`.
* [ ] Nothing prints to the screen behind the TUI. Any stray `println!` corrupts the
      display (§23).
* [ ] Run inside `tmux` and over SSH at 80×24, 100×30, and 160×48.
* [ ] Run with `--ascii` in a terminal with no Unicode support, and with
      `NO_COLOR=1`.

Metric honesty, on a machine where you can check against something else:

* [ ] Memory figures are consistent with `free -h` (Linux) or Activity Monitor
      (macOS) *as defined in* [`metrics.md`](metrics.md). Different numbers are
      expected; undocumented different numbers are a bug.
* [ ] An unavailable metric reads as unavailable, never as `0`. Try a container with
      a cgroup limit, and a Wi-Fi link with no reported speed.
* [ ] A stale value is visibly marked and carries its age.

Safety (§15.1):

* [ ] No destructive action is reachable by a single keypress.
* [ ] Process actions are refused while the timeline is not live.
* [ ] Signalling a process that has already exited reports that, and never signals
      whatever now holds the PID.

Distribution:

* [ ] `monitrs --version` prints the version you are releasing.
* [ ] `monitrs manpage | man -l -` renders.
* [ ] Completions load in your shell: e.g.
      `monitrs completions zsh > /tmp/_monitrs && fpath=(/tmp $fpath) && compinit`.
* [ ] `monitrs config init` writes a file, `monitrs config check` accepts it, and
      neither overwrites an existing one.

## 7. Commit, tag, push

```sh
git switch -c release/vX.Y.Z
git add Cargo.toml Cargo.lock CHANGELOG.md README.md
# 1.0.0 only: the semver job becomes blocking in this same commit (step 2).
git add .github/workflows/ci.yml
git commit -m "release: vX.Y.Z"
```

Open a pull request, let CI pass, and merge it. Then, on the merged commit:

```sh
git switch main
git pull --ff-only
git tag -a vX.Y.Z -m "monitrs vX.Y.Z"
git push origin vX.Y.Z
```

Two rules:

* **Tag the merged commit, not your branch.** The archives are built from the tag,
  and a tag on an unmerged commit ships code that is not on `main`.
* **Never move or delete a published tag.** Checksums and provenance are bound to
  the commit it pointed at. A mistake gets a new patch version — see
  [Rollback](#rollback).

Before pushing the tag you can dry-run the whole workflow without publishing
anything. `workflow_dispatch` builds and assembles, and the publish step is gated on
`github.event_name == 'push'`:

```sh
gh workflow run release.yml --ref main -f tag=vX.Y.Z
gh run watch
```

The dry run validates the tag string against the manifest of the ref you pass, so it
catches a version mismatch before a tag exists.

## 8. What the workflow verifies for you

`.github/workflows/release.yml`, on a `v*` tag push. You do not need to repeat any
of this by hand.

**`verify` — fails before any artifact exists:**

* the tag, with `v` stripped, equals the `monitrs` version from `cargo metadata`;
* `CHANGELOG.md` has a section for that version;
* `cargo fmt --all -- --check`;
* `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
* `cargo test --workspace --all-features --locked`.

**`build` — once per target, for all six §19.2 targets** (`x86_64` and `aarch64`
Linux gnu and musl, `x86_64` and `aarch64` macOS):

* `cargo build --release --locked --target <target>`;
* the built binary runs `--version` and `--help`, except for `x86_64-apple-darwin`,
  which is cross-built on an arm64 runner and cannot be executed there;
* an archive is assembled containing the binary, `README.md`, both licences, a
  changelog **excerpt** for this version, shell completions for bash, zsh, fish,
  PowerShell and elvish, and a man page. Completions and the man page are generated
  from the binary being shipped, so they cannot disagree with its real flags;
* a per-archive `.sha256`.

**`publish` — the only job with write permissions:**

* collects every `.sha256` into one `SHA256SUMS`;
* attaches a build-provenance attestation to each archive (§18.4);
* extracts the release notes from the changelog and appends verification
  instructions;
* creates the GitHub release, marked pre-release for any `0.x` version.

Every action is pinned to a commit SHA, and the top-level token is read-only.

**What it does not check:** anything in step 6. Terminal restoration, metric
honesty, and process-action safety are human work.

## 9. Verify what was published

Download from the release page — not from a build artifact — into an empty
directory:

```sh
gh release download vX.Y.Z --pattern '*.tar.gz' --pattern 'SHA256SUMS'
```

Checksums:

```sh
# Linux
sha256sum --check --ignore-missing SHA256SUMS

# macOS
shasum -a 256 --check --ignore-missing SHA256SUMS
```

`--ignore-missing` is what lets you check one archive without downloading all six.
Every line checked must print `OK`.

Provenance — this is what proves the archive came from this workflow, from this
commit, rather than from someone's laptop:

```sh
gh attestation verify monitrs-X.Y.Z-<target>.tar.gz --repo <owner>/monitrs
```

Read the output rather than the exit code alone: it names the workflow and the
commit that produced the file. Confirm the commit is the one the tag points at:

```sh
git rev-parse vX.Y.Z^{commit}
```

Then check the archive is what it claims to be:

```sh
tar tzf monitrs-X.Y.Z-<target>.tar.gz
./monitrs-X.Y.Z-<target>/monitrs --version
```

* [ ] all six archives present, plus `SHA256SUMS`;
* [ ] every checksum verifies;
* [ ] provenance verifies for at least one archive per OS, and names the tagged
      commit;
* [ ] the archive contains the binary, `README.md`, `LICENSE-MIT`,
      `LICENSE-APACHE`, `CHANGELOG-excerpt.md`, `completions/`, and `man/`;
* [ ] the extracted binary reports the released version;
* [ ] the release body contains the changelog section, not the whole file;
* [ ] a `0.x` release is marked pre-release.

## 10. Publish to crates.io

Last, and only after the tag and its archives are verified: crates.io publication cannot be
undone. A version can be **yanked**, which stops new dependents from selecting it, but the
files stay downloadable forever and the version number can never be reused.

The four crates publish in dependency order, because each is verified by building it
against the registry copies of the others:

```
monitrs-core  ->  monitrs-collectors  ->  monitrs-tui  ->  monitrs
```

`cargo publish --workspace` does that ordering, and the index wait between crates, itself:

```sh
cargo login                     # paste the token at the prompt, not on the command line
cargo publish --workspace --locked --dry-run
cargo publish --workspace --locked
cargo logout
```

Three rules:

* **The token never appears in `argv`.** `cargo publish --token <value>` puts it in the
  process table, where every other user on the machine can read it. `cargo login` prompts.
* **`--dry-run` first.** It packages and verifies every crate without uploading, which
  catches an excluded file, an oversized package, or a dependency that is not published.
* **`cargo logout` afterwards.** The token lands in `~/.cargo/credentials.toml` in plain
  text; there is no reason for it to outlive the release.

Then verify what the registry actually serves, in an empty directory, from the registry
rather than from your checkout:

```sh
cargo install monitrs --locked --version X.Y.Z --root /tmp/monitrs-verify
/tmp/monitrs-verify/bin/monitrs --version
```

* [ ] all four crates show the new version on crates.io;
* [ ] `docs.rs` built the documentation for each — a failed docs.rs build is invisible from
      here and is the most common thing to go wrong after a successful publish;
* [ ] `cargo install monitrs --locked` fetches and builds the new version.

## 11. After publishing

* [ ] `CHANGELOG.md` on `main` has an empty `Unreleased` section again.
* [ ] Any documentation that names a version matches.
* [ ] Installation methods documented in the README actually work for this release —
      run them, from a downloaded archive, in an empty directory:
      * the checksum command in the README must name `SHA256SUMS`. The workflow
        concatenates the per-archive `.sha256` files into that one file and then `rm -f
        ./*.sha256`, so a README telling the reader to check
        `monitrs-X.Y.Z-<target>.tar.gz.sha256` sends them to a file the release does not
        carry. That was true of `0.1.0`'s README for its whole life.
      * `cargo install monitrs --locked`, which step 10 has just verified.
* [ ] **The Homebrew tap carries this version.**
      [`gaborini/homebrew-monitrs`](https://github.com/gaborini/homebrew-monitrs) is a
      separate repository, so nothing here updates it automatically. Each release needs
      the version and four `sha256` values in `Formula/monitrs.rb` changed, from this
      release's own `SHA256SUMS`, and then:

      ```sh
      brew update && brew upgrade monitrs && brew test monitrs
      ```

      Automating it would mean a cross-repository write token in *this* repository's
      secrets, which §18.4 forbids, so the trade has not been made. Until it is, a
      release that skips this step leaves `brew install monitrs` on the previous version
      with no sign that it is behind.
* [ ] Deb, RPM, Nix and other packaging are later milestones and must not block a
      release.

## Rollback

A published release cannot be un-published safely. The remedy is always forward.

1. Do not move the tag. Do not delete it. Someone may already have the archives and
   a provenance record bound to that commit.
2. Mark the GitHub release as a draft or add a prominent warning to its body
   describing the defect and what to use instead.
3. Fix the defect on `main`, bump the patch version, and follow this checklist
   again.
4. Record the defect in `CHANGELOG.md` under the new version, and say which version
   was withdrawn.

## The §23 gate

§23's list, verbatim, as a checklist. A box is ticked only with evidence — a test, a
recorded measurement, or a named machine — and an unticked box is a thing this project
owes, carried into each release's *Known limitations* rather than resolved by ticking it.

- [x] **launch, monitor, filter, inspect, pause, seek, return live, and quit all
      work.** Exercised by hand in a real terminal, and by
      `crates/monitrs/tests/integration.rs` (13 tests over the assembled application)
      and `crates/monitrs/tests/capture.rs`, which renders frames from the live
      collector — the frames in `README.md` and `docs/screenshots/` are its output.
- [x] **process actions are safe.** The confirmation chain and identity revalidation
      are tested in the reducer, the signal path has live tests, and
      `scripts/verify-renice.py` drives the *interface* through a renice on a process
      it starts itself and then asks `ps` — not monitrs — whether the value changed.
- [x] **terminal restoration is reliable.** `scripts/verify-terminal-restoration.py`
      runs the release binary on a real pty, stops it five ways, and checks both the
      escape sequences and the pty's `termios`: `q`, `Ctrl-C`, `SIGTERM`, `SIGHUP`, and
      a provoked panic. The first run of it found `SIGTERM` and `SIGHUP` leaving the
      terminal in raw mode — the process died before the guard could drop — which
      `runtime::spawn_signal_thread` now fixes by routing them into the ordinary
      shutdown. A panic could not be provoked through the interface; the hook is
      covered by unit tests.
- [x] **Linux and macOS Tier 1 builds pass.** All seven jobs of the release workflow
      succeeded for `v0.1.0`: the tag verification and six target builds — Linux x86_64
      and aarch64 in glibc and musl, macOS on both architectures. macOS x86_64 is a
      cross-compile because GitHub's Intel runners are being retired, so CI builds it
      and does not run it.
- [x] **release archives and checksums are published.** The workflow has run for a real
      tag: `v0.1.0` has six archives and a `SHA256SUMS` on its release page, every
      checksum verified against the downloaded files, and every archive carrying a build
      attestation that `gh attestation verify` accepts.
- [ ] **default settings remain below the memory and CPU budgets on the reference
      workload.** Frame time, input latency, collection p95, resident memory **and the
      idle-CPU median on macOS** are measured and pass. **§16.1's twelve-hour run is
      met** — 2026-08-02, both Tier 1 Linux architectures, memory and descriptors both,
      inside the seven-day window the [step 5](#5-soak) deferral set. The **idle-CPU
      budget does not pass, and on §16.1's own reference workload neither half of it
      does**: median 2.66% against 1% and p95 3.99% against 2%, at 8 CPUs and 199–200
      processes. On the 12-core Mac at a thousand processes the median passes at
      0.60–0.85% and the p95 fails at 4.30–9.50%. The cause is the medium tier's two
      filesystem-capacity reads, at 13.2–35.0 ms of CPU per tick against a whole-tick
      budget of roughly 16 ms — *not* the 85 ms temperature read this list previously
      named, which turns out to be almost all blocked wait and to cost a few
      milliseconds of CPU at most. This box
      used to predict that the reference workload would be "about five times smaller".
      It was read on 2026-08-02 and it is **worse**, not smaller — fewer processes did
      not make it cheaper, and the readings turn out to be quantised in 1.33% steps, so
      a passing median is not a value the instrument can report. Numbers,
      breakdown and the commands that produce them are in
      [`benchmarks.md`](benchmarks.md#the-161-end-to-end-budgets). Step 6 above
      now has that reference-workload reading as a step, with the protocol in
      [`benchmarks.md`](benchmarks.md#reading-the-idle-cpu-budget-on-its-own-reference-workload);
      it has not been run.
- [x] **every claim in the README is supported by a test or a documented
      measurement.** §20.1's required contents are all present: the demo frame is
      real and reproducible, every performance figure links to the file that produced
      it, and the row that fails its budget is in the same table as the ones that
      pass. What remains unverified is the install instructions for the four archives
      nobody has run — the two macOS ones have been run from the published tarball.
      §20.1's list, all present and checked: value proposition, a real captured
      frame, honest differentiation, supported platforms, install methods,
      keybindings, configuration path, the metric-semantics warning, the privilege
      model, the privacy statement, development commands, and the licence. No
      fabricated benchmarks. No mocked screenshots.

And the per-feature clause of §23, which applies to everything in the release:
behaviour implemented end to end; error, empty, unsupported, permission and
warming-up states handled; keyboard behaviour documented; unit or integration tests
over the core logic; UI snapshots for material visual states; Linux/macOS
differences documented; formatter, clippy, tests, docs and build passing; no debug
printing in the TUI; no hidden unsafe or destructive behaviour; performance impact
measured where the sampling loop is touched; and the docs updated.
