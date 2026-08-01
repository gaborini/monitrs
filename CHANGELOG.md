# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries describe what a *user* can observe. Anything not listed here does not
work yet, regardless of what the source contains.

## [Unreleased]

### Fixed

* **`monitrs snapshot --format json | head` no longer reports an error.** Closing the
  pipe is the reader's decision, and monitrs treated it as a failure: it printed
  `monitrs: Broken pipe (os error 32)` and exited non-zero, so under the
  `set -o pipefail` that careful scripts use, an ordinary pipeline failed. It now exits
  quietly and successfully, and records the reason in `--debug-log` at debug level.
  Only `snapshot` could reach this in practice — its export is about 800 KB, larger than
  any pipe buffer, so it is still writing after the reader has gone. `completions`
  (17 KB), `manpage` (4 KB) and `config` (under 100 bytes) fit in the buffer and finish
  writing first, which is why nobody had seen it there. Their handling is fixed too:
  `config` printed with `println!` and `completions` used a helper that writes straight
  to stdout, and both of those *panic* rather than return an error when the write fails,
  so had the pipe ever broken the result would have been a panic report. Both now route
  through the same path as `snapshot`.

  The recognition is deliberately narrow — the failure must itself be the broken pipe.
  A broken pipe reachable only as some other error's cause is still reported and still
  exits non-zero, because that one did not come from writing monitrs' own output, and
  silently exiting 0 on a run that really failed would be worse than the message this
  replaces.

### Documentation

* **`docs/soak-testing.md`'s independent memory check was measuring the wrong process.**
  The page asks for a resident-size reading taken from outside the soak, so the figure
  does not rest solely on monitrs' own measurement, and then gave `pgrep -f soak-` —
  which also matches the `tee` the same page tells you to pipe through. It is not a coin
  flip: `pgrep` prints ascending PIDs, `head -1` takes the first, the `tee` exists from
  the moment the pipeline starts, and cargo spawns the test binary minutes later once
  the build finishes. So the corroborating reading was of the one process guaranteed not
  to grow. The pattern is now anchored to `^[^ ]*/deps/soak-`, verified on Amazon Linux
  2023 and macOS, and the page says how to confirm it matched exactly one process.

## [1.0.0] - 2026-08-01

The stability promise. Four surfaces are frozen and each has a machine guard behind it.
On the way there the sensors came off the one-size-fits-all schedule, and a reading
carried over from an earlier tick now says how old it is instead of being republished as
though it had just been measured.

### Added

#### Sensors read on their own cadence, and a carried-over reading is dated

* **Temperatures and the battery are read every 30 seconds instead of every 5** — and
  every 5 seconds while the Battery screen is visible, plus immediately when you open it.
  They used to ride the 5-second medium tier whatever was on screen. On macOS one such
  read costs about 85 ms of wall clock, which is a long time to spend on a number nobody
  is looking at. It did **not** fix the budget it was aimed at, and *Known limitations*
  below says so rather than leaving you to find out.
* **A reading carried over from an earlier tick is published as stale, carrying its age.**
  Until now a value read four seconds ago was republished as freshly measured, and nothing
  downstream could tell the difference. At a 30-second cadence that would have become a
  half-minute-old temperature presented as current, which is the one thing this project
  will not do. The rule reaches the interface, the JSON export, and a library consumer
  reading `SensorSnapshot` alike.
* **The header dates its hottest reading**: `temp 62.5C ~00:28`. The alternative was a
  field that vanished for most of every 30 seconds, which is what filtering to freshly
  measured readings would have done — a dated number beats a blank.
* **The Battery screen states the age once per panel**, in the panel's own trailing label
  — `82% discharging ~00:28`, `2 sensors ~00:28`. A dozen fields unwrapped from one
  retained snapshot are all exactly the same age, and `~00:28` printed beside each of them
  would be one fact rendered twelve times; the fields themselves are drawn in the stale
  style rather than as measurements. The charge meter is the exception, because every
  meter annotates its own value where it draws it.

#### The Time Lens caret says what changed, not only when

* **A caret parked on a historical sample now carries the sample's wall clock and how its
  CPU compares with two baselines**, then how far back it is: `22:14:07Z`, then
  `cpu prev +41 points`, then `30s -6 points`, then `-00:37 selected`. §2.5's comparisons
  have been in the core since `0.1.0` with nothing calling them; this is where they reach
  a screen. A baseline the history cannot reach reads `30s no baseline` rather than `+0`,
  because a zero delta would say the metric did not change when the truth is that there
  was nothing to compare it against.
* **The segments are ordered by what nothing else carries**, and the note drops from the
  end when the caret's side of the row runs out of room. The wall clock leads: between 80
  and 99 columns this note is the only place the *selected* sample's clock appears at all,
  because the compact header's one-line strip reads the live snapshot. The relative offset
  goes last, since the `[<HISTORY -MM:SS]` badge repeats it at every width.

#### What `1.0.0` froze, and the four guards behind it

* **Four surfaces are frozen**: the public API of `monitrs-core`, `monitrs-collectors` and
  `monitrs-tui`; the JSON export; the configuration keys; and the default keymap.
  [`CONTRIBUTING.md`](CONTRIBUTING.md)'s *What 1.0.0 froze* is the reference rather than
  this list — it states the terms of each, what is deliberately **not** frozen (layout,
  wording, colour, glyph choice and panel arrangement are presentation, not API), and why
  no one of the four guards can see another's blind spot.
* **Each surface has a mechanism, not a paragraph.** The API is checked by
  `cargo-semver-checks` in CI; the export by `docs/schema/v2.json`, an inventory of all 292
  field paths version 2 produces, and `crates/monitrs/tests/schema_contract.rs`; the
  configuration keys by `docs/schema/config-v1.json`, all 32 of them, and
  `crates/monitrs/tests/config_contract.rs`; the keymap by exact key→action assertions in
  `crates/monitrs-tui/src/keymap.rs`. Three of the four fail a build today. The semver job
  reports without blocking until the commit that tags `v1.0.0`, which is where it becomes
  the gate.

### Changed

Breaking for library users, one at a time:

* **Six public enums are now `#[non_exhaustive]`**: `Effect`, `Action`, `ViewId` and
  `SortField` (`monitrs-tui`), the palette `Command` (`monitrs-tui`), and `HistoryMetric`
  (`monitrs-core`). Matching one of them from another crate now needs a wildcard arm,
  which is itself the break — and this is the last moment it can be taken, because the
  attribute is what buys the whole of `1.x` a new screen, a new effect, a new sortable
  column, a new palette command or a new retained metric without a major bump.
  **`MetricState` is deliberately excluded**: there a consumer's exhaustive match is the
  protection rather than an inconvenience, and a new availability state *should* cost a
  major bump, because every one of them has to be handled deliberately. (`FilterPattern`
  in `monitrs-core` carried the attribute already, so seven public enums carry it in
  total; `CONTRIBUTING.md` lists all seven with their file paths.)
* **`SparklineCaret::with_note` is replaced by `with_note_segments`**, which takes the note
  as priority-ordered segments instead of one finished string. Only the widget knows which
  side of the caret the note lands on and how much room that side has, so only the widget
  can decide what fits; a caller sizing one string against the row's full width produced
  notes that vanished outright for most scrub positions on a 160-column Overview.
* **`Effect::SetSensorInterest(bool)` is a new variant** — how the visible screen tells the
  sampler to read the sensors every 5 seconds instead of every 30.
* **`SensorSnapshot::hottest` is deprecated rather than removed.** Its `fresh()` filter
  returns `None` for exactly the retained readings that became normal in this release, so
  it is now a trap rather than a convenience; the header's own use of it was the first
  casualty. It still compiles and still means what it always meant, and its deprecation
  note names the replacement, which yields the value *together with* its age. Removal
  waits for a later major version, per the deprecation policy in `CONTRIBUTING.md`.

And two changes that break nothing and still need saying:

* **The JSON export's usual shape changed, and `schema_version` is still `2`.** No field
  was removed and none was renamed, so the version does not move — but the *variant
  frequency inverted*. Sensors are stale most of the time now, so a consumer of the command
  palette's `export snapshot <path>` that reads `sensors.temperatures.available` used to
  find it on every tick and will now usually find `sensors.temperatures.stale` instead,
  with the reading under `.value` and its age beside it. The same goes for
  `sensors.battery`. Both shapes are recorded in `docs/schema/v2.json` and both are
  guarded. The `monitrs snapshot --format json` subcommand is unaffected: it takes a single
  sample with every tier due, so its sensors are always freshly measured.
* **`sampling.slow_interval` and `sampling.medium_interval` govern different things now**,
  with no key added, removed or renamed. `slow_interval` (30 s) sets the sensor read while
  nobody is looking at a sensor panel; `medium_interval` (5 s) sets it only while the
  Battery screen is open, where it used to set it always. A configuration file written
  before `1.0.0` still loads and still means what it says about the tiers.
  [`docs/configuration.md`](docs/configuration.md) is the record, because a frozen key
  whose *meaning* moves is precisely the thing none of the four guards can see.

### Fixed

* **Below 92 columns the tab strip lost every screen name, including the active one's** —
  a regression `0.2.0` listed in its own *Known limitations*. The active screen keeps its
  title, bracketed, and only the other six condense to bare digits, so the one piece of
  chrome that says where you are survives the narrow bands. The threshold is computed from
  the room the footer's key hints leave rather than compared against a constant, so it
  moves with them: 92 columns with the two hints a live view shows, 100 while the timeline
  is frozen and a third appears.

### Known limitations

Compiled by re-reading `0.2.0`'s list and checking each item against the code and
[`docs/benchmarks.md`](docs/benchmarks.md), rather than by copying it forward — plus what
this release itself leaves open. Every figure below comes from that file.

* **The idle self-CPU p95 still misses §16.1's budget, and closing it is what this release
  was built for.** Median **0.60–0.85%** against 1%, which passes; p95 **4.30–9.50%**
  against 2%, which does not. The previous figures were 0.5–1.1% and 6–11%, so the p95
  improved and the two median ranges overlap — "barely moved" is the honest reading of the
  median. With the Battery screen visible, where the sensors return to five seconds, the
  median is clearly worse (**1.20–1.70%**) while the p95 (**6.00–8.30%**) does not separate
  from the Overview row's at all. §16.1's gate is about idle, and idle is the Overview row.
* **The cause this release was designed around turned out to be the wrong one**, which is
  the most useful thing this section can tell you. The plan assumed the ~85 ms `sysinfo`
  temperature read on the 5-second tier was what the budget was paying for. A thread- and
  process-CPU instrument built afterwards bounded that read at about **4 ms of CPU**: the
  85 ms is a blocking wait on in-kernel IOKit calls, and §16.1 budgets CPU. What is
  measured to cost CPU is the medium tier's remaining work — its two filesystem-capacity
  reads — at **13.2–35.0 ms beyond a fast-only tick, positive in 15 of 15 runs**, against
  a whole-tick budget of roughly 16 ms. Which of the two reads carries it was never
  separated, and where the sensor read's 85 ms actually goes is a hypothesis rather than a
  finding. The sensor work stands on its own merits regardless: the wall-clock cost is
  real, a 136 ms tick occupies the sampler thread whether or not it burns CPU, and a
  retained reading that says its age is right whatever the budget does.
* **Every figure in this release was measured against 981–1007 processes**, five times
  §16.1's 200-process, 8-CPU reference workload, and the dominant costs scale with process
  count. The budget has never been read on the workload it was written for.
  `scripts/measure-overhead.py` can now take that reading, and prints the host's real
  process count beside its verdict so it cannot call a mismatched host a reference
  measurement. The run is owed; `docs/benchmarks.md` says in advance what each outcome
  means.
* **The twelve-hour soak was deferred until after this release, by decision.** It is a
  blocking gate, and it did not lapse: on 2026-08-01 the maintainer decided to tag `1.0.0`
  first and run the soak after, and `docs/release-checklist.md`'s step 5 records the
  decision, its reasoning, and the deadline — all three runs within seven days of the tag.
  What exists instead is a 30-minute run with the shipped collector, which shows no growth:
  resident size fell, retained history stayed bounded, file descriptors were flat at 3, and
  nothing was dropped. §16.1 asks for twelve hours, so that is the right shape rather than
  the gate. The trade being made is that a leak is a `1.0.1` patch and cannot touch what
  this release froze. What the trade does **not** cover is that **nothing has been soaked
  on Linux at all**, which is also the only configuration in which the descriptor budget is
  exercised — so the Linux run is the one carrying real unknown. If the soak finds
  something, it will appear in `1.0.1`'s notes and in `docs/soak-testing.md`.
* **Two of the seven screens have never been seen on Linux.** The Battery screen's Linux
  collector is tested from captured `/sys/class/power_supply` fixtures and the inode reader
  from `statfs` errno cases, both on macOS; no Linux machine has drawn either. Every frame
  in `docs/screenshots/` is a macOS frame.
* **Four of the six published archives have never been executed.** Every archive's checksum
  and build attestation verifies, and the two macOS ones have been run from the published
  tarball — the x86_64 one only under Rosetta, where it reports temperatures as
  unsupported. Nobody has run the four Linux archives on their own hardware. Unchanged
  since `0.1.0`.
* A slow terminal can still block a frame for an unbounded time, and no instrument here can
  see it: frame time is measured through ratatui's `TestBackend`, which stops short of the
  write to the terminal, and the soak harness has no renderer. Whether anything constitutes
  a redraw busy loop, as opposed to the idle-redraw interval the reducer enforces, is
  likewise unmeasured.
* Device busy time remains unsupported on macOS: there is no documented API for it. On
  macOS the Battery screen's cycles, capacity, wear, pack temperature and watts read `n/a`
  for the related reason — they live in the undocumented `AppleSmartBattery` registry
  properties §9.3 forbids this build from reading — and are real on Linux.
* PSI is Linux-only, and says `n/a` on macOS rather than promising a value that will never
  arrive.
* Timestamps are UTC and labelled `Z`: no time-zone database is bundled.

## [0.2.0] - 2026-07-30

Two more screens, the container facts the collector was already reading, and a way to
watch one process together with everything it spawned.

### Added

#### Two new screens

* **A CPU screen at `3`**, which renumbers Storage to `4`, Network to `5` and Inspect to
  `6`. It groups the per-core meters by **core class** — `PERFORMANCE` and `EFFICIENCY` on
  Apple Silicon, and whatever else a platform names — because four efficiency cores at 90%
  with four performance cores idle is a machine doing very little, and the opposite is a
  machine working hard, from the same eight numbers. The load average is shown per logical
  CPU beside it, since `11.4` means different things on 4 cores and on 128. A machine that
  reports one kind of core gets one panel, which is a fact rather than a missing feature.
* **A Battery screen at `7`.** Charge, state, time remaining *only where the platform
  reports it* — never derived from a rate — cycle count, current capacity against design
  capacity with the resulting wear, pack temperature, and draw in watts. Plus a thermal
  sensor panel: those readings previously reached the screen as a single figure in the
  Overview header. A machine with no battery says so once, in the panel label, rather than
  filling a screen with placeholders. **On macOS, cycles, capacity, wear, pack temperature
  and watts all read `n/a`**: they live in the undocumented `AppleSmartBattery` registry
  properties that §9.3 forbids this build from reading. The charge, the state and the time
  remaining are real; the rest is honest absence, and the Linux collector reports all of it.
* **The Linux battery collector**, from `/sys/class/power_supply/*` on the medium tier.
  `docs/platform-support.md` had been promising it. The system battery is picked by
  `type == Battery` with `scope` absent or `System`, which is what excludes the charger and
  a wireless mouse's cell; amp-hour drivers are converted through `voltage_min_design`, and
  without that the capacity stays unavailable rather than being guessed.

#### Follow one process with its children

* **`F` scopes the process table to the selected process and everything beneath it**, and
  `F` again lifts the scope. The palette has `follow [pid]` and `unfollow`. The panel title
  says `following 410` while it is on, because four rows out of a thousand with no reason on
  screen is indistinguishable from a monitor that has lost the other processes.
* **The trailing label carries what the whole family costs** —
  `4 of 10 total, cpu >=107%, rss 479M`. That figure is the point: a build's compilers come
  and go every second, so no individual row ever answers "what is this build using", and a
  text filter on `cc` finds every compiler on the machine including someone else's.
* **`>=` marks a sum as a lower bound.** A member whose CPU the OS refused is still counted
  as a member and still shown with `denied` in its cell, but it cannot contribute to the
  total. A sum with *no* contributors reports the members' own state, so an all-refused
  family reads `denied` rather than `0%`.
* **Membership is downwards only** — the process and its descendants, never its parent — and
  the root is a `(pid, start key)` pair, so a recycled PID cannot quietly become the thing
  being followed. When the root exits, monitrs stops following and says so rather than
  silently following the orphans that init inherited. Losing only children changes nothing.

#### Container awareness on Linux

* **The cgroup CPU ceiling is shown beside the host's CPU count**, never instead of it:
  `8 logical, 8 physical, cgroup 1.5 CPUs` on Inspect, `cgroup 1.5 cpu` in the header, and
  the same figure in the CPU screen's title. A group limited to 1.5 CPUs on a 64-CPU machine
  is not "2% of the machine"; it is a wall its processes are throttled against. An unlimited
  group is *unsupported* rather than a very large number of CPUs, and a `cpu.max` that cannot
  be parsed is *unavailable* rather than "no limit", because those mean opposite things to
  someone trying to explain a stall.
* **The cgroup's own memory charge**, from `memory.current` — the counter the kernel compares
  against `memory.max` when it decides to OOM-kill — beside the limit it is enforced against:
  `cgroup limit 2.0G, 512M used (25%)`. `/proc/meminfo` is not namespaced, so the `memory`
  row is the *host's*: a process in a 2 GiB group on a 64 GiB host reads the host's 40 GiB
  and concludes it is nearly out of memory when it has used 300 MiB of its allowance. Both
  halves of that ratio now come from the group, or neither does.
* **The container is named where the cgroup path identifies one** — `container docker
  3f4a1b2c9d8e`, or `kubernetes/containerd …` — on the Inspect screen's environment row,
  ahead of the evidence, so a narrow panel truncates "how we guessed" rather than "what this
  is". A `/.dockerenv` file is evidence of a container that names no container, and that case
  stays unnamed rather than being filled in with a placeholder.
* **The load average is deliberately *not* divided by the cgroup quota.** `/proc/loadavg` is
  not namespaced either: inside a container it counts every runnable process on the machine,
  including other tenants'. The divisor stays the host's CPU count and the label becomes
  `over 8 host cores`, so the figure is visibly about the machine. Inventing a container's
  saturation by division would be worse than not having it.

#### The Storage screen

It had two panels and thirty blank rows. It now has four.

* **A `TOP DISK I/O` panel** ranking processes by read and write rate, with cumulative
  totals. Two ordering rules do the work: a process whose counters were refused sorts *below*
  a measured idle one, because a refusal is not zero; and where rates tie — which on a real
  machine is nearly every process — the tie-break is the cumulative totals rather than the
  PID. Without the second rule the panel is twenty-eight rows of `0B/s` in launch order.
* **Inode usage** per filesystem, from `getfsstat` on macOS and `statfs(2)` on Linux. A
  filesystem with no inode table reports *unsupported*, never `0 of 0`, and a refused read
  reads `denied` rather than collapsing into the `n/a` that means "no inode table".
* **Mounts that share a device are marked**, with the panel stating that `SIZE is not
  additive`. An APFS Mac lists the same disk under several mount points, and adding those
  figures up is a mistake the screen used to invite.
* **A throughput history panel** from the retained ring, labelled as the machine aggregate,
  because the ring keeps no per-device series and fabricating one is not allowed.

#### Open files and sockets

* **The process detail overlay names the descriptors**, not just counts them: file, socket,
  pipe, event queue, shared memory, semaphore, with the path where there is one. Capped at
  256 per process, with the number not listed stated rather than the list silently ending.
* **Socket counts on macOS** flipped from unsupported to available. The old comment claimed
  a syscall per descriptor; `PROC_PIDLISTFDS` already carries the type of every descriptor,
  so the count costs 3 µs where reading the paths costs 216 µs — measured, on a
  442-descriptor process.

#### Pressure escalations are announced

* **A Pressure Radar signal changing state now produces a notice** quoting the diagnostics
  engine's own rule text, not a paraphrase. Recoveries are announced too, at `info`
  severity, because a signal that goes quiet without a word looks the same as one that
  stopped being collected. An unavailable signal is
  never a de-escalation: it clears the remembered state, so the samples either side of a
  permission error cannot be stitched into a change that never happened.
* **`diagnostics.bell_on_critical`** (default `false`) rings the terminal bell once per
  sample on escalation into critical. Configuring it while `diagnostics.enabled = false` is
  reported as the contradiction it is.

### Changed

* **`TemperatureReading::high_celsius` is now `peak_celsius`** — a breaking change for
  library users. It was documented as the sensor's declared high threshold, but the
  underlying interface reports a high-water mark on macOS, and the two are
  indistinguishable from inside the collector. A bar scaled against a high-water mark sits
  at 100% forever. Only `critical_celsius` is a declared ceiling, and it is now the only
  thing a thermal scale may use.
* **`BatterySnapshot::health` is a method rather than a field**, derived from the new
  `capacity`, so a wear percentage can no longer disagree with the two capacities printed
  beside it. Also breaking for library users.
* **`LinuxEnrichment::cgroup_cpu_limit` returns a `MetricState<CpuQuota>`** instead of an
  `Option<CpuMax>`, and `MeminfoSnapshot::to_snapshot` takes the group's charge alongside
  its limit. The fields those changes added are in the consolidated list below.
* **`MemorySnapshot::effective_limit_bytes` now accepts a stale cgroup reading**, which is
  the one place this codebase deliberately breaks its own "fresh values for calculations"
  rule. A limit is configuration, not a measurement: if the last good read said 2 GiB and
  this tick's failed, falling back to the host's 64 GiB advertises 62 GiB of headroom that
  does not exist. `CpuSnapshot::effective_cores` is its new counterpart and follows the same
  rule; it is new in this release rather than changed.

* **The JSON export schema is version `2`.** `monitrs snapshot --format json` reports
  `"schema_version": 2`, and a consumer that checks it — which is what the field is for —
  will now refuse rather than misread. Two fields were **removed**:
  `sensors.battery.health` and `sensors.temperatures[].high_celsius`. The second's
  replacement, `peak_celsius`, deliberately means something else, so a script reading the
  old name as a declared limit would have silently reinterpreted a high-water mark as one.
  The old `health` is `capacity.full_microwatt_hours / capacity.design_microwatt_hours`.
  Added, which alone would not have earned a bump: `cpu.cgroup_quota`, `cpu.core_classes`,
  `memory.cgroup_used_bytes`, `filesystems[].inodes`,
  `sensors.battery.{capacity, temperature_celsius, power_watts}`, and
  `host.environment.available.container` on Linux.
* **Six public enums gained variants**, and none of them is `#[non_exhaustive]`, so a
  downstream `match` no longer compiles until it grows an arm:
  `monitrs_core::process::ProcessPredicate::InSubtree`, `monitrs_tui::ViewId::{Cpu, Battery}`,
  `monitrs_tui::Action::{FollowSelected, StopFollowing}`, `monitrs_tui::Effect::RingBell`,
  `monitrs_tui::app::Command::{Follow, Unfollow}`, and
  `monitrs_tui::app::NoticeKind::Pressure`. Two public constants changed type as a
  consequence: `ViewId::ALL` is now `[ViewId; 7]` and `NoticeKind::ALL` is `[NoticeKind; 8]`,
  so a spelled-out array length fails to compile.

  These types stay exhaustive on purpose. `Effect` in particular is matched without a
  wildcard in the one function that acts on the outside world, and that exhaustiveness is
  what forces a new effect to be wired to something real instead of silently doing nothing.
* **Nine public fields were added to seven public structs**, breaking struct literals.
  Four have no `Default`, so *every* literal breaks: `HostEnvironment::container`,
  `FilesystemSnapshot::inodes`, `ProcessDetail::open_file_list`, and
  `macos::MachineFacts::core_classes`. Three have one, so only fully exhaustive literals
  break: `Scenario::{asymmetric_cores, process_open_files, battery_state}`,
  `linux::LinuxSources::power_supplies`, and `app::AppSettings::bell_on_critical`. Add to
  those `CpuSnapshot::{cgroup_quota, core_classes}`, `MemorySnapshot::cgroup_used_bytes`,
  and `BatterySnapshot::{capacity, temperature_celsius, power_watts}`.
* **The screens renumbered, so `ViewId::to_digit` and `ViewId::from_digit` answer
  differently.** Inserting CPU at `3` moved Storage to `4`, Network to `5` and Inspect to
  `6`; `5` now opens Network for anyone with the old key in their fingers or their scripts.
* **Below 92 columns the tab strip shows bare digits instead of screen names** — `[1] 2 3 4
  5 6 7` rather than `[1 Overview] 2 Processes …`. Seven titles need 76 cells beside the
  footer hints, where five needed 58, so an 80-column terminal that used to show the names
  no longer can. While the timeline is frozen the footer carries `L live` too and the
  cutover rises to 100 columns, which reaches into the Standard band. The active screen
  stays bracketed, and §5.2's rule that colour is never the only carrier still holds.

### Fixed

* **Storage listed one row per mount point rather than per device**, so an APFS Mac showed
  the same disk four times with four identical throughput figures — and a reader adding
  them up got four times the machine's real I/O. One row per device now, with every mount
  point it backs named in the row.

* **`OPEN FILES` on macOS was the descriptor table size, not the count.** A process
  reporting 25 held 3 descriptors, opened 20 more, and still reported 25; another reporting
  1600 held 453. It now comes from the enumeration.
* **`ROOT` in the process detail rendered as an empty cell for every process on the
  machine**, because the kernel fills `pvi_rdir` only when it differs from `/`. It is now
  `/`, and never the empty string.
* **Sub-absolute-zero temperatures reached the screen.** Every Apple Silicon Mac reports
  about eight unwired `PMU tdev*` sensors at roughly −9200 °C, and the new battery screen
  printed them as temperatures. The collector discards them; 25 readings become 17 real ones
  on the machine this was found on.
* **A failed `cpu.max` read kept the previous value**, presenting a ceiling read minutes
  earlier as the current one. An unparsable limit is now unavailable.
* **`cargo doc` was broken on Linux** — a public module doc in the new `statfs` reader
  linked to a private constant — and nothing on a macOS workstation could see it, because
  neither `cargo check --target` nor `clippy --target` runs rustdoc. The release checklist
  now runs all three against the other platform.
* **Three documents still sent the reader to `5` for the Inspect screen** after the
  renumbering: the README, the troubleshooting guide and the accessibility review.
* **The command palette's `view` hint and its rejection message listed only the five 0.1.0
  screens**, so `cpu` and `battery` worked but were undiscoverable. The rejection now names
  every screen, and a test walks `ViewId::ALL` so the next screen cannot be added without
  one.

### Known limitations

Carried forward and re-checked rather than copied: what 0.1.0 listed and this release still
owes, minus what is now fixed.

* **The twelve-hour soak is still not on record**, and it is now explicitly out of scope
  here: it will be run on a dedicated EC2 host under its own project rather than on a
  workstation, because the gate §16.1 asks for is twelve uninterrupted hours and a laptop
  that sleeps does not produce one. The 30-minute run with the shipped collector remains
  the only evidence, and it showed no growth. **Nothing has been soaked on Linux**, which
  is also the only configuration where the file-descriptor budget is exercised at all.
* **Idle self-CPU still misses its p95 budget**: median 0.5–1.1% against 1% — met — and p95
  6–11% against 2%. `docs/benchmarks.md` locates the cost read by read; it is OS reads
  rather than monitrs' own computation.
* **Two of the seven screens have never been seen on Linux.** The Battery screen's Linux
  collector is tested from captured `/sys/class/power_supply` fixtures on macOS, and the
  inode reader from `statfs` errno cases, but no Linux machine has run either.
* A slow terminal can still block a frame for an unbounded time, and no instrument here can
  see it.
* **Below 92 columns the tab strip loses the screen names**, which is new in this release
  and is the cost of going from five screens to seven.
* Device busy time remains unsupported on macOS: there is no documented API. Per-process
  socket counts, which 0.1.0 listed here, are now supported — one syscall already carried
  them.
* PSI is Linux-only, and says `n/a` on macOS rather than promising a value that will never
  arrive.
* Timestamps are UTC and labelled `Z`: no time-zone database is bundled.
* Of the six published archives, only the two macOS ones have ever been run, and the
  x86_64 one only under Rosetta. That is unchanged from 0.1.0.

## [0.1.0] - 2026-07-30

First release. `monitrs` shows what a machine is doing now and what it was doing a
few minutes ago, and it is explicit about the difference between a metric it
measured, one it is still warming up, one the OS refused, and one this platform
cannot produce.

### Added

#### The interface

* **Five screens** — Overview, Processes, Storage, Network, Inspect — and six
  overlays: help, command palette, filter edit, sort selector, process detail, and
  the process-action confirmation that covers both signals and renice. Notices are a
  panel rather than an overlay, and the spike attribution is the body of the Time Lens
  screen rather than something you open. Keyboard-first throughout; the mouse is
  optional and off by default.
* **The Time Lens.** `Space` pauses the visible timeline without stopping
  collection, `[` and `]` scrub through the retained history, and `L` returns to
  live. Selecting a historical sample shows which processes accounted for the
  change, with an explicit statement of how much of the change the named processes
  account for — correlation, described as correlation.
* **The Pressure Radar.** Each signal carries the rule text that produced it, the
  raw metric it was derived from, and how long the state has held. Hysteresis stops
  it flapping. No rule claims a diagnosis the data cannot support: no OOM, no memory
  leak, no disk failure, no malware, no thermal throttling.
* **A stable process table.** Sorting is total, with a `(pid, start key)`
  tie-breaker, so a hundred rows all reporting `0%` keep their order between
  refreshes. The cursor follows the process you chose rather than the row it was
  on; until you choose one it tracks the top of the table, so the busiest process is
  what a fresh session shows. Tree mode re-attaches the children of a filtered-out
  parent instead of scattering them.
* **Four layout bands** from 160×48 down to 80×24, and below that a minimal process
  list rather than a broken frame. Every panel's width is reserved from the layout,
  so no value can push a column out of alignment.
* **Strict ASCII mode** (`--ascii`) whose output is seven-bit by assertion, a
  Unicode mode, three themes, and `--color off`/`NO_COLOR`. Colour is never the only
  carrier of meaning: every state also has a distinct character, and every
  unavailable value says which kind of unavailable it is — `warming up`,
  `permission denied`, `n/a`, or a specific reason — abbreviated but never merged
  as the column narrows.

#### What it measures

* **Per-metric availability.** `MetricState<T>` makes "no value" a state rather
  than a zero, and a retained value cannot be displayed without its age.
* **Native enrichment by default.** On Linux, `/proc` and `/sys`: PSI, cgroup v2
  limits reported separately from host totals, device busy time, per-process I/O,
  and a start key with clock-tick resolution. On macOS, documented APIs only —
  `sysctl`, `host_statistics64`, `host_processor_info`, `proc_pidinfo`,
  `getifaddrs`, and IOKit's public power interfaces — with wired and compressed
  pages reported separately. No external commands and no private APIs on either.
* **Rates are computed from measured intervals**, never from an assumed one, and a
  counter that went backwards reports a reset rather than a spike.
* **Process identity is a PID plus a start key**, so a reused PID inherits nothing:
  not the selection, not a pin, and not a pending action.

#### Doing something about it

* **Signals** — `x` opens the dialog, `T` proposes SIGTERM, `K` proposes SIGKILL —
  behind a confirmation that names the process. A signal never follows from one
  keypress, and the forceful ones demand a distinct key rather than `Enter`, so
  leaning on the confirm key cannot escalate. The identity is rechecked immediately
  before delivery, so a PID reused between the dialog and the write is refused
  instead of signalled.
* **Renice** (`R`) on both platforms, with the same revalidation. A dry run says in
  advance whether the value will be permitted; lowering a nice value needs
  privileges monitrs never acquires, and it says so rather than failing silently.
* **Process actions are refused while the timeline is away from live**, because the
  process on a frozen screen is not necessarily the process that PID names now.
* **No privilege escalation, ever.** monitrs never invokes `sudo`, never asks for a
  password, and reports what it could not read.

#### Around the edges

* `monitrs snapshot --format json` with command arguments redacted by default,
  `monitrs config path`/`init`/`check`, shell completions for five shells, and a
  manpage — the last two generated from the same definition the program parses.
* **Versioned TOML configuration** that rejects an unknown key rather than ignoring
  it, points at the exact key for an out-of-range value, suggests the near miss for
  a typo, and detects key conflicts. Reload is atomic and applies to the running
  interface *and* the sampler; the settings that need a restart are named.
* `--debug-log` on every subcommand, carrying collector durations and the
  dropped/coalesced counts, and never writing to a terminal that is showing the
  interface.
* **The terminal is restored** on quit, on `Ctrl-C`, on `SIGTERM`, on `SIGHUP`, on
  error, and on panic — before the panic report is printed, and before slow workers
  are joined. A signal reaches the ordinary shutdown path rather than killing the
  process where it stands, so `kill` gives the terminal back instead of leaving it
  needing `reset`. Verified on a real pty by
  `scripts/verify-terminal-restoration.py`, which checks the escape sequences *and*
  the pty's `termios` state for each case.

### Measured

§16.1's budgets, measured rather than asserted. Frame render, input latency and
collection come from `crates/monitrs/tests/capture.rs`; self CPU, resident memory
and descriptors from `scripts/measure-overhead.py`, which observes the running
binary from outside. Full numbers and the per-read breakdown are in
[`docs/benchmarks.md`](docs/benchmarks.md#the-161-end-to-end-budgets).

| Budget | Measured on a 12-core Mac, ~1000 processes |
|---|---|
| frame render below 16 ms at 160×48 | median 200 µs, p95 353 µs |
| input-to-visible-response below 50 ms | median 417 µs, p95 486 µs |
| sample collection below 200 ms p95 | ordinary tick p95 15–21 ms; every fifth tick 121–161 ms |
| resident memory below 50 MiB | median 29 MiB, peak 31 MiB |
| no unbounded growth | 30-minute soak: resident size *fell*, descriptors flat, nothing dropped |

2224 tests. Among them: 86 recorded frames of the rendered interface across three
snapshot suites, covering ASCII, Unicode, no-colour, and the empty,
permission-denied, stale and warming-up states; 120 Linux `/proc` and `/sys` fixtures
that run on every platform because each parser takes bytes rather than a path;
property tests over the formatters and the sort; 13 integration tests over the
assembled application; and a soak harness that drives the real worker threads.

### Known limitations

Named because §16.1's own last line asks for measurement rather than claims:

* **The idle self-CPU budget is not met.** Median 1.3–2.7% against a 1% target, and
  p95 11–15% against 2%, on a host with about a thousand processes — five times
  §16.1's reference workload. The cost is OS reads, not monitrs' own computation,
  which is three orders of magnitude smaller; `docs/benchmarks.md` locates it read by
  read and says what would close it.
* **No twelve-hour soak is on record.** A 30-minute run with the shipped collector
  is, and shows no growth. The twelve-hour run is the actual gate, and nothing has
  been soaked on Linux.
* A slow terminal can block a frame for an unbounded time, and no instrument here can
  see it: the frame-time measurement renders through ratatui's `TestBackend`, which
  stops short of the write to the terminal, and the soak harness has no renderer.
* Per-process socket counts and device busy time are unsupported on macOS: the first
  costs one syscall per descriptor, the second has no documented API. Both say
  `n/a` rather than guessing.
* PSI is Linux-only, and says `n/a` on macOS rather than promising a value that will
  never arrive.
* Timestamps are UTC and labelled `Z`: no time-zone database is bundled.
* Of the six published archives, only the two macOS ones have been run — and the
  x86_64 one only under Rosetta on Apple Silicon, where it reports temperatures as
  unsupported. Every archive's checksum and build attestation verifies; nobody has run
  the four Linux archives or an Intel Mac build on its own hardware.

### Changed

* Relicensed from GPL-3.0 to dual MIT OR Apache-2.0 before any release, matching
  the Rust ecosystem norm and this project's dependency licence policy. Anyone who
  cloned the repository at its first commit saw the earlier licence.

[Unreleased]: https://github.com/gaborini/monitrs/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/gaborini/monitrs/releases/tag/v1.0.0
[0.2.0]: https://github.com/gaborini/monitrs/releases/tag/v0.2.0
[0.1.0]: https://github.com/gaborini/monitrs/releases/tag/v0.1.0
