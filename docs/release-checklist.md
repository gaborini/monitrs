# Release checklist

Everything needed to cut a monitrs release, in order, written for someone who has
never done it. Follow it top to bottom. Nothing here is optional except where it
says so.

## Read this first: `0.1.0` is not releasable yet

**The interface launches**, and the §23 verbs — launch, monitor, filter, inspect,
pause, seek, return live, quit — have each been exercised by hand in a real
terminal. So has the renice flow, by `scripts/verify-renice.py`: it drives the real
binary on a pty, presses `R` on a process it started itself, and then asks `ps` —
not monitrs — whether the value changed. What is still missing before `0.1.0`:

* **No twelve-hour soak is on record.** A 30-minute run with the shipped collector
  is ([`soak-testing.md`](soak-testing.md#runs-on-record)): resident size fell over
  the run, descriptors stayed at 3, retained history stayed bounded with the ring
  full, and nothing was dropped even with the channel saturated. That is evidence,
  not the gate — §16.1 says twelve hours. (The 90 ms worst-case input latency that run
  reported turned out to be the harness measuring two thread wake-ups rather than
  monitrs; see [`soak-testing.md`](soak-testing.md).)
* **The idle self-CPU budget of §16.1 is not met.** Measured on a 12-core Mac with
  about a thousand processes: median 1.3–2.7%, p95 11–15%, against a budget of 1% and
  2%. The median is close; the p95 is not.
  The other five measurable budgets pass — frame render, input latency, collection
  p95, resident memory, descriptor growth — and
  [`benchmarks.md`](benchmarks.md#the-161-end-to-end-budgets) has the numbers, the
  per-read breakdown showing where the time goes, and what it would take to fix.
  This is the one item on this list that is a *product* problem rather than a
  procedural one.
* **The release notes are written** — `CHANGELOG.md` has a `0.1.0` section and
  `changelog_excerpt.py` extracts it — but **the date in its heading is
  `2026-07-30`**. Keep a Changelog dates a section by its release date, so if the tag
  is pushed on another day, change that line first. The workflow's `verify` job does
  not check the date, only that the section exists.

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

So this file is currently a **procedure, not a plan**. It is complete and it is
followed as written, but the [§23 gate](#the-23-gate-for-010) at the bottom is what
decides whether a tag may be pushed, and today it does not pass. Do not cut `0.1.0`
by ignoring it. If an early tag is genuinely wanted for packaging or CI work, tag it
`v0.0.x`, keep the pre-release flag the workflow already applies to `0.x`, and say
plainly in the release notes that the interactive interface is absent.

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
* nothing else. **No publishing token belongs on your machine or in this
  repository** (§18.4). The release workflow uses the run's own `GITHUB_TOKEN`, and
  crates.io publication is not part of `0.1.0` (§19.3).

Optional: `just`, a thin wrapper around the cargo commands below. Every recipe
prints the command it runs, and this checklist gives the underlying commands so
`just` is never required.

## 1. Decide the version

* SemVer (§19.1). Pre-`1.0`, a breaking change bumps the minor.
* Tags are `vX.Y.Z`. The release workflow triggers on `v[0-9]+.[0-9]+.[0-9]+*`, so
  `v0.1.0-rc.1` also triggers it.
* Anything `0.x` is published as a **pre-release** automatically, so nobody mistakes
  it for a stability promise.
* Do **not** publish to crates.io until the name and metadata are final (§19.1).

## 2. Bump the version

The version lives in **exactly four places**, and three of them are one file. Every
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

Today, with only an `Unreleased` section, it correctly fails:

```text
$ python3 .github/scripts/changelog_excerpt.py 0.1.0
CHANGELOG.md has no section for 0.1.0
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

Also confirm the dependency graph has not drifted:

```sh
cargo tree -p crossterm --workspace     # exactly one crossterm major version (§13)
```

## 5. Soak

Blocking. [`soak-testing.md`](soak-testing.md) has the invocations, how to read the
report, and what to record. In short:

* [ ] twelve-hour release-profile run, report kept;
* [ ] one-hour 10,000-process run, report kept;
* [ ] one-hour real-collector run **on Linux** — the only configuration in which the
      file-descriptor budget is actually exercised, because the macOS collector opens
      no files;
* [ ] all three reports attached to the release record, with machine, toolchain, and
      profile.

## 6. Verify by hand, on real machines

Automation cannot do these. Do them on **both** a Linux machine and a macOS
machine, and write down the version and architecture of each.

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
git add Cargo.toml Cargo.lock CHANGELOG.md
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

## 10. After publishing

* [ ] `CHANGELOG.md` on `main` has an empty `Unreleased` section again.
* [ ] Any documentation that names a version matches.
* [ ] Installation methods documented in the README actually work for this release:
      the archives, and `cargo install monitrs --locked` **only once crates.io
      publication has happened** (§19.3 — not for `0.1.0`).
* [ ] A Homebrew tap is a later step, deliberately: §19.3 says after the release
      process has stabilised. One release is not stability.
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

## The §23 gate for 0.1.0

§23's list, verbatim, as a checklist. `0.1.0` may not be tagged until every box is
ticked with evidence — a test, a recorded measurement, or a named machine.

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
- [x] **release archives and checksums are published.** The workflow does this; it
      has not yet run for a real tag. The `aarch64-apple-darwin` archive has been
      assembled and exercised by hand (step 8 below).
- [ ] **default settings remain below the memory and CPU budgets on the reference
      workload.** Frame time, input latency, collection p95, resident memory,
      descriptor growth **and the idle-CPU median** are measured and pass. The
      **idle-CPU p95 does not**: 6–11% against a 2% budget, which is the medium
      tier's 85 ms temperature read arriving as one spike every five seconds. Nothing
      has been measured on §16.1's actual reference workload (8 CPUs, 200 processes),
      where the per-process costs would be about five times smaller. Numbers,
      breakdown and the commands that produce them are in
      [`benchmarks.md`](benchmarks.md#the-161-end-to-end-budgets).
- [x] **every claim in the README is supported by a test or a documented
      measurement.** §20.1's required contents are all present: the demo frame is
      real and reproducible, every performance figure links to the file that produced
      it, and the row that fails its budget is in the same table as the ones that
      pass. What remains unverifiable until a release exists is the install
      instructions for the five archives nobody has run.
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
