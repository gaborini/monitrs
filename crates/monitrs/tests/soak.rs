//! The soak harness: proof that the runtime does not grow or stall (§16.1, §16.2).
//!
//! §16.1 asks for two things no unit test can show — **no unbounded memory growth
//! over a twelve-hour run** and **no unbounded file-descriptor growth** — and §16.2
//! asks for three more under load: input stays responsive, snapshots coalesce, and
//! queues never grow without bound.
//!
//! A twelve-hour test cannot run in CI, so this one is *scalable*: the run length
//! comes from the environment and defaults to a few seconds. The same code that a
//! developer smokes in ten seconds is the code that runs for twelve hours before a
//! release. `docs/soak-testing.md` is the operator's half of this file: how to run
//! the real thing, and what to record.
//!
//! ```text
//! cargo test -p monitrs --test soak -- --ignored --nocapture
//! MONITRS_SOAK_SECONDS=43200 cargo test --release -p monitrs --test soak -- --ignored --nocapture
//! ```
//!
//! # What is actually under test
//!
//! The real worker threads from `src/runtime.rs`, driven against
//! [`FakeCollector`] with a scenario twenty times §16.1's reference workload,
//! feeding the real reducer and the real history ring:
//!
//! ```text
//!   sampler ──┐
//!   tick    ──┼──> bounded channel ──> drain_to_newest_snapshot ──> apply() ──> AppState
//!   detail  ──┤                                                                 └─ HistoryRing
//!   input   ──┘  (simulated: see below)
//! ```
//!
//! Nothing here is a mock. A soak test against a stubbed channel would prove only
//! that the stub does not leak.
//!
//! The fake collector is the default because it makes the load a knob and the run
//! reproducible — but it opens no files, so on its own it can only show that the
//! *runtime* leaks no descriptors. `MONITRS_SOAK_REAL_COLLECTOR=1` swaps in
//! [`platform_collector`], which is the code that actually opens things, and is what a
//! pre-release soak should run at least once. See [`SourceKind`].
//!
//! # Why the module source is included rather than imported
//!
//! `monitrs` is a binary crate, so it has no library target for an integration test
//! to link against, and `runtime.rs` is deliberately `pub(crate)`. `#[path]` puts
//! the real module into this test binary, which is the only way to drive it without
//! widening its visibility for the sake of a test. `logging.rs` comes along because
//! `runtime.rs` calls into it, not because the soak run logs anything.
//!
//! That has one visible consequence: `cargo test` sets `cfg(test)` for an
//! integration target too, so those modules' own unit tests are compiled into this
//! binary and run again here under `runtime::tests::*` and `logging::tests::*`. They
//! are fast, and having the soak binary re-verify the channel invariants it depends
//! on is no loss.
//!
//! # Why the input thread is simulated
//!
//! `spawn_input_thread` calls `crossterm::event::poll`, which needs a real tty; a
//! test that spawned it would either hang or assert nothing. The injector below
//! sends the same [`Event::Terminal`] values through the same [`EventSender`], so
//! the property §16.2 cares about — a keypress is not delayed or lost by a flood of
//! snapshots — is measured end to end. What is not covered here is crossterm's own
//! event translation, which `monitrs-tui` tests directly.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              soak failure must be loud"
)]

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use monitrs_collectors::fake::Scenario;
use monitrs_collectors::selfstat::{SELF_MEASUREMENT_COMPILED, SelfUsage};
use monitrs_collectors::{FakeCollector, platform_collector};
use monitrs_core::history::{HistoryConfig, MIN_HISTORY_DURATION, MIN_SAMPLE_INTERVAL};
use monitrs_core::model::ProcessIdentity;
use monitrs_tui::app::{AppSettings, AppState, apply};
use monitrs_tui::event::{Event, KeyPress, TerminalEvent};

// `runtime.rs` records the collector's duration and the channel's losses through
// `crate::logging` (§14.2), so the same `#[path]` trick has to bring that module in
// too or the module would not compile here. No subscriber is installed in this
// binary, so every one of those calls is the no-op dispatch a run without
// `--debug-log` gets — which is also the configuration §16.1's budgets assume.
#[path = "../src/logging.rs"]
mod logging;
#[path = "../src/runtime.rs"]
mod runtime;

use runtime::{
    ChannelHealth, DetailRequest, EVENT_CHANNEL_CAPACITY, EventSender, SampleRequest,
    SamplingControl, Shutdown, Workers, detail_channel, drain_to_newest_snapshot, event_channel,
    spawn_detail_worker, spawn_sampler_thread, spawn_tick_thread,
};

/// The event payload type. The soak run never reloads configuration, so the
/// reducer's opaque `Cfg` parameter is the unit type.
type SoakEvent = Event<()>;

// ---------------------------------------------------------------- configuration

/// How long to run. Seconds. `43200` is the twelve hours §16.1 names.
const ENV_SECONDS: &str = "MONITRS_SOAK_SECONDS";
/// The fast-tier sampling interval, in milliseconds.
const ENV_INTERVAL_MS: &str = "MONITRS_SOAK_INTERVAL_MS";
/// How many processes the fake system has.
const ENV_PROCESSES: &str = "MONITRS_SOAK_PROCESSES";
/// Set to `1` to soak the real platform collector instead of the fake one.
const ENV_REAL_COLLECTOR: &str = "MONITRS_SOAK_REAL_COLLECTOR";

/// Default run length: long enough to reach a steady state and take a dozen
/// measurements, short enough that CI's `--ignored` pass does not notice it.
const DEFAULT_SECONDS: u64 = 10;
/// Default fast-tier interval. Fifty samples a second is fifty times §16.1's
/// reference rate: the point of a soak is to compress a day into an hour.
const DEFAULT_INTERVAL_MS: u64 = 20;
/// Default process count: twenty times §16.1's reference 200.
///
/// Not §16.2's 10,000, and deliberately. A synthetic 10,000-process system costs
/// close to 300 MiB resident in a debug build, which is a lot to ask of a shared CI
/// runner for a test that is not the release gate. `docs/soak-testing.md` gives the
/// 10,000-process invocation as a longer, deliberate run — it is the configuration
/// in which the UI genuinely sheds load, and it is worth doing on purpose rather
/// than by default.
const DEFAULT_PROCESSES: usize = 4_000;

/// How many footprint measurements to aim for across the measurement phase.
///
/// Twenty-four gives six per quartile, which is enough for a mean to mean
/// something without turning a twelve-hour run into a needlessly large series.
const TARGET_MEASUREMENTS: u32 = 24;
/// The fewest measurements a run may produce and still be interpretable.
const MIN_MEASUREMENTS: usize = 8;
/// Never measure more often than this: the reading itself costs a syscall or two.
const MIN_MEASURE_EVERY: Duration = Duration::from_millis(100);
/// Never measure less often than this, however long the run is.
const MAX_MEASURE_EVERY: Duration = Duration::from_secs(60);

/// Growth in resident bytes that is tolerated between the first and last
/// quartiles, on top of a 25% relative allowance.
///
/// Not a fudge factor. Neither glibc's nor macOS's allocator returns freed pages
/// to the OS promptly, so a flat workload still shows single-digit megabytes of
/// drift before it settles — measured on macOS 26 at the defaults, resident size
/// climbed about three megabytes over the first ten seconds and was then flat to
/// the kilobyte for the following thirty.
///
/// What justifies the size is the scale a *real* leak has here. One retained
/// snapshot at the default 4,000 processes is on the order of a megabyte, and the
/// sampler produces fifty a second, so leaking anything the sampling loop touches
/// crosses this tolerance in well under a second. A tolerance large enough to
/// absorb allocator drift is still three orders of magnitude below the failure it
/// is looking for.
const RESIDENT_TOLERANCE_BYTES: u64 = 16 * 1024 * 1024;

/// Descriptor growth tolerated between the first and last quartiles.
///
/// Small on purpose: descriptors are not subject to allocator hysteresis, so the
/// honest expectation is exactly flat. The slack covers a reading that races a
/// short-lived file opened elsewhere in the process.
const DESCRIPTOR_TOLERANCE: u32 = 4;

/// The least warm-up allowance, however short the requested run.
///
/// The measurement phase cannot begin until the history ring is full, and filling
/// it takes `capacity` samples — which on a slow machine can take longer than a
/// ten-second run. The run is allowed to overshoot rather than to fail: an honest
/// slow machine and a broken build must not produce the same result.
const WARM_UP_FLOOR: Duration = Duration::from_secs(30);

/// A history ring this long is used for runs shorter than this threshold.
///
/// Below it, the shipped 300-sample ring could not fill *and* then demonstrate a
/// flat trend, so the smallest supported ring is used instead. At or above it, the
/// configuration under test is the one that ships.
const SHIPPED_HISTORY_FROM: Duration = Duration::from_secs(300);

/// Interval between simulated keypresses.
///
/// Frequent enough to produce a useful latency sample, rare enough that the UI
/// thread is measured while it is busy with snapshots rather than with keys.
const KEY_INTERVAL: Duration = Duration::from_millis(50);
/// How long the injector waits for the UI to acknowledge a keypress before
/// recording it as unanswered.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Ceiling on the *median* input latency.
///
/// §16.1's budget is 50 ms input-to-visible-response at 200 processes in a release
/// build. This is a debug build at six times the process count and forty times the
/// sample rate, so the budget itself is not the assertion; what is asserted is that
/// input is still answered in a fraction of a second while the reducer is
/// saturated, which is the §16.2 property. `docs/soak-testing.md` explains how to
/// measure the real budget.
const MEDIAN_LATENCY_CEILING: Duration = Duration::from_millis(400);
/// Ceiling on the worst single input latency, past which the UI counts as stalled.
const MAX_LATENCY_CEILING: Duration = Duration::from_secs(3);

/// Which collector the run drives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceKind {
    /// [`FakeCollector`]: deterministic, and it touches no file at all.
    ///
    /// The right default — the run is reproducible and the load is a knob — but it
    /// means the descriptor curve proves that the *runtime* leaks no descriptors,
    /// not that the platform collector leaks none. It cannot: it opens nothing.
    Fake,
    /// [`platform_collector`]: the real thing, native enrichment and all.
    ///
    /// The mode that actually tests §16.1's descriptor budget, because this is the
    /// code that opens files. Slower, and not reproducible, so it is opt-in.
    Real,
}

impl SourceKind {
    /// A short label for the report.
    const fn label(self) -> &'static str {
        match self {
            Self::Fake => "fake",
            Self::Real => "real",
        }
    }
}

/// One parsed soak configuration.
#[derive(Clone, Copy, Debug)]
struct SoakConfig {
    /// Total run length, excluding warm-up overshoot and shutdown.
    duration: Duration,
    /// The sampler's fast-tier interval.
    sample_interval: Duration,
    /// Processes in the fake system. Ignored by [`SourceKind::Real`], which sees
    /// whatever the machine is actually running.
    processes: usize,
    /// Which collector to drive.
    source: SourceKind,
    /// Cadence of footprint measurements during the measurement phase.
    measure_every: Duration,
    /// Latest point at which the measurement phase begins, even if the history
    /// ring is not yet full.
    warm_up_cap: Duration,
    /// The history configuration under test.
    history: HistoryConfig,
}

impl SoakConfig {
    /// Reads the environment, falling back to the smoke-test defaults.
    ///
    /// An unparsable or zero value falls back rather than failing: a typo in a
    /// twelve-hour invocation should not be discovered twelve hours later, and the
    /// resolved configuration is printed in the report either way.
    fn from_env() -> Self {
        let duration = Duration::from_secs(read_env(ENV_SECONDS).unwrap_or(DEFAULT_SECONDS).max(1));
        let sample_interval = Duration::from_millis(
            read_env(ENV_INTERVAL_MS)
                .unwrap_or(DEFAULT_INTERVAL_MS)
                .max(1),
        );
        let processes = read_env(ENV_PROCESSES)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(DEFAULT_PROCESSES)
            .max(1);

        let source = match std::env::var(ENV_REAL_COLLECTOR).ok().as_deref() {
            Some("1" | "true" | "yes") => SourceKind::Real,
            _ => SourceKind::Fake,
        };

        let measure_every =
            (duration / TARGET_MEASUREMENTS).clamp(MIN_MEASURE_EVERY, MAX_MEASURE_EVERY);
        // Half the run, or thirty seconds, whichever is longer. Half so that a long
        // run spends most of itself measuring; thirty seconds so that a short run on
        // a slow machine overshoots instead of failing.
        let warm_up_cap = (duration / 2).max(WARM_UP_FLOOR);
        let history = if duration >= SHIPPED_HISTORY_FROM {
            HistoryConfig::default()
        } else {
            HistoryConfig {
                interval: MIN_SAMPLE_INTERVAL,
                duration: MIN_HISTORY_DURATION,
                ..HistoryConfig::default()
            }
        };

        Self {
            duration,
            sample_interval,
            processes,
            source,
            measure_every,
            warm_up_cap,
            history,
        }
    }
}

/// Reads a `u64` from the environment, treating anything unparsable as absent.
fn read_env(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse::<u64>().ok()
}

// -------------------------------------------------------------------- the report

/// One footprint measurement, taken on the UI thread between events.
#[derive(Clone, Copy, Debug)]
struct Measurement {
    /// Offset from the start of the measurement phase.
    at: Duration,
    /// Our own resident set size.
    resident_bytes: u64,
    /// Descriptors we held.
    descriptors: u32,
    /// Snapshots the UI had processed by this point, so a stalled sampler is
    /// visible as a flat count rather than only as a flat memory curve.
    snapshots_seen: u64,
    /// Bytes the history ring reported retaining.
    history_bytes: usize,
}

/// Everything one run observed.
#[derive(Debug)]
struct SoakReport {
    /// The configuration that produced it.
    config: SoakConfig,
    /// How long the run actually took, warm-up included.
    elapsed: Duration,
    /// How long the warm-up phase took.
    warm_up: Duration,
    /// The footprint series.
    measurements: Vec<Measurement>,
    /// Snapshots the UI processed in total.
    snapshots: u64,
    /// The last snapshot sequence the UI saw.
    last_sequence: u64,
    /// Detail replies the UI received.
    details: u64,
    /// Simulated keypresses the UI acknowledged, with their round-trip latency.
    latencies: Vec<Duration>,
    /// Keypresses that were never acknowledged.
    unanswered: u64,
    /// The deepest the event channel was ever observed.
    peak_queue: usize,
    /// Snapshots superseded before being drawn, by the sender and by the UI.
    coalesced: u64,
    /// Non-coalescable events lost because the channel stayed full.
    dropped: u64,
    /// Coalesced count before the deliberate stall.
    coalesced_before_stall: u64,
    /// Coalesced count after it.
    coalesced_after_stall: u64,
    /// Channel depth observed at the end of the stall.
    queue_after_stall: usize,
    /// History samples retained at the end, and the ring's capacity.
    history: (usize, usize),
    /// The worst-case size the ring itself admits for that capacity.
    ///
    /// Taken from the ring rather than computed here: §8.5 makes the ring
    /// responsible for its own bound, and a second ceiling invented by a test would
    /// eventually disagree with it.
    history_ceiling: usize,
    /// Workers spawned, and the names of any that failed to join cleanly.
    workers: (usize, Vec<&'static str>),
}

impl SoakReport {
    /// Mean resident bytes over a slice of the series.
    fn mean_resident(window: &[Measurement]) -> u64 {
        let count = u128::try_from(window.len()).unwrap_or(1).max(1);
        let total: u128 = window
            .iter()
            .map(|point| u128::from(point.resident_bytes))
            .sum();
        u64::try_from(total / count).unwrap_or(u64::MAX)
    }

    /// Highest descriptor count over a slice of the series.
    fn peak_descriptors(window: &[Measurement]) -> u32 {
        window
            .iter()
            .map(|point| point.descriptors)
            .max()
            .unwrap_or(0)
    }

    /// The first and last quartiles of the series.
    ///
    /// Quartiles rather than first-versus-last reading: a single reading can catch
    /// the allocator mid-growth, and a trend is what §16.1 asks about.
    fn quartiles(&self) -> (&[Measurement], &[Measurement]) {
        let len = self.measurements.len();
        let quarter = (len / 4).max(1);
        let first = self.measurements.get(..quarter).unwrap_or(&[]);
        let last = self.measurements.get(len - quarter..).unwrap_or(&[]);
        (first, last)
    }

    /// The median observed input latency.
    fn median_latency(&self) -> Duration {
        if self.latencies.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted = self.latencies.clone();
        sorted.sort_unstable();
        sorted
            .get(sorted.len() / 2)
            .copied()
            .unwrap_or(Duration::ZERO)
    }

    /// The worst observed input latency.
    fn max_latency(&self) -> Duration {
        self.latencies
            .iter()
            .copied()
            .max()
            .unwrap_or(Duration::ZERO)
    }
}

impl fmt::Display for SoakReport {
    /// The block `docs/soak-testing.md` asks the operator to paste into the release
    /// record. Printed rather than logged so `--nocapture` is all it takes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (first, last) = self.quartiles();
        writeln!(f, "--- monitrs soak report ---")?;
        writeln!(
            f,
            "run:            {:?} requested, {:?} elapsed ({:?} warm-up)",
            self.config.duration, self.elapsed, self.warm_up
        )?;
        writeln!(
            f,
            "collector:      {} ({} processes requested)",
            self.config.source.label(),
            self.config.processes
        )?;
        writeln!(
            f,
            "load:           {:?} fast tier, history {} of {} samples, ceiling {} B",
            self.config.sample_interval, self.history.0, self.history.1, self.history_ceiling
        )?;
        writeln!(
            f,
            "snapshots:      {} processed, last sequence {}, {} detail replies",
            self.snapshots, self.last_sequence, self.details
        )?;
        writeln!(
            f,
            "channel:        peak depth {} of {}, {} coalesced, {} dropped",
            self.peak_queue, EVENT_CHANNEL_CAPACITY, self.coalesced, self.dropped
        )?;
        writeln!(
            f,
            "stall probe:    depth {} of {} after the stall, coalesced {} -> {}",
            self.queue_after_stall,
            EVENT_CHANNEL_CAPACITY,
            self.coalesced_before_stall,
            self.coalesced_after_stall
        )?;
        writeln!(
            f,
            "input:          {} keys, median {:?}, worst {:?}, {} unanswered",
            self.latencies.len(),
            self.median_latency(),
            self.max_latency(),
            self.unanswered
        )?;
        writeln!(
            f,
            "resident:       first quartile {} KiB, last quartile {} KiB, peak {} KiB",
            Self::mean_resident(first) / 1024,
            Self::mean_resident(last) / 1024,
            self.measurements
                .iter()
                .map(|point| point.resident_bytes)
                .max()
                .unwrap_or(0)
                / 1024
        )?;
        writeln!(
            f,
            "descriptors:    first quartile peak {}, last quartile peak {}",
            Self::peak_descriptors(first),
            Self::peak_descriptors(last)
        )?;
        writeln!(
            f,
            "workers:        {} spawned, {} failed to join",
            self.workers.0,
            self.workers.1.len()
        )?;
        writeln!(f, "measurements ({}):", self.measurements.len())?;
        for point in &self.measurements {
            writeln!(
                f,
                "  {:>9?}  rss {:>8} KiB  fds {:>4}  snapshots {:>8}  history {:>7} B",
                point.at,
                point.resident_bytes / 1024,
                point.descriptors,
                point.snapshots_seen,
                point.history_bytes
            )?;
        }
        write!(f, "--- end of report ---")
    }
}

// ----------------------------------------------------------------- the injector

/// Everything the injector thread shares with the UI thread.
#[derive(Debug)]
struct Injector {
    /// Round-trip latency of every acknowledged keypress.
    latencies: Arc<std::sync::Mutex<Vec<Duration>>>,
    /// Keypresses the UI never acknowledged.
    unanswered: Arc<AtomicU64>,
    /// Set when the thread has left its loop, so the UI can wait for it before
    /// deliberately stalling.
    finished: Arc<AtomicBool>,
}

impl Injector {
    /// The shared state, before the thread exists.
    fn new() -> Self {
        Self {
            latencies: Arc::new(std::sync::Mutex::new(Vec::new())),
            unanswered: Arc::new(AtomicU64::new(0)),
            finished: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The simulated input thread: sends a keypress and times the acknowledgement.
///
/// Registered as a [`Workers`] entry so that it is joined with the rest, which
/// also proves `join_all` handles a worker the runtime did not spawn itself.
fn spawn_input_injector(
    workers: &mut Workers,
    sender: EventSender<()>,
    acks: crossbeam_channel::Receiver<Instant>,
    stop: Shutdown,
    shared: &Injector,
) -> std::io::Result<()> {
    let latencies = Arc::clone(&shared.latencies);
    let unanswered = Arc::clone(&shared.unanswered);
    let finished = Arc::clone(&shared.finished);
    workers.spawn("soak-input", move || {
        while !stop.is_triggered() {
            let sent_at = Instant::now();
            // `k` is bound to "select previous row" (§6.2), so the reducer resolves
            // the key and moves the selection. An unbound key would measure the
            // channel and nothing else.
            if !sender.send(Event::Terminal(TerminalEvent::Key(KeyPress::char('k')))) {
                break;
            }
            match acks.recv_timeout(ACK_TIMEOUT) {
                Ok(_) => {
                    let latency = sent_at.elapsed();
                    if let Ok(mut latencies) = latencies.lock() {
                        latencies.push(latency);
                    }
                }
                Err(_) => {
                    // Either the UI is gone or it took longer than the timeout.
                    // Counted rather than panicked: the assertion belongs on the
                    // main thread, where a failure can print the whole report.
                    unanswered.fetch_add(1, Ordering::Relaxed);
                    if stop.is_triggered() {
                        break;
                    }
                }
            }
            std::thread::sleep(KEY_INTERVAL);
        }
        finished.store(true, Ordering::Release);
    })
}

/// Applies one event, acknowledging a keypress only once the reducer has run.
///
/// The acknowledgement is what the latency measurement stops on, so it must come
/// *after* `apply`: §16.1's budget is input-to-visible-response, not
/// input-to-dequeued.
fn absorb(state: &mut AppState, event: SoakEvent, acks: &Sender<Instant>, details: &mut u64) {
    let was_key = matches!(event, SoakEvent::Terminal(_));
    if matches!(event, SoakEvent::Detail(_)) {
        *details += 1;
    }
    let _ = apply(state, event);
    if was_key {
        // `try_send` because at most one keypress is ever outstanding: a full
        // acknowledgement channel would mean the injector already has its answer.
        let _ = acks.try_send(Instant::now());
    }
}

// ---------------------------------------------------------------------- the run

/// Runs one soak and returns what it observed.
///
/// Structured as three phases so that each assertion has data it can trust:
/// a warm-up until the history ring is full, a measurement phase, and a
/// deliberate UI stall that forces the coalescing path §16.2 requires.
#[allow(
    clippy::too_many_lines,
    reason = "one linear procedure; splitting it would hide the phase order that \
              makes the measurements interpretable"
)]
fn run_soak(config: SoakConfig) -> SoakReport {
    let started_at = Instant::now();
    let (sender, receiver) = event_channel::<()>();
    let health: ChannelHealth = sender.health();
    let shutdown = Shutdown::new();
    let input_stop = Shutdown::new();
    let mut workers = Workers::new();

    let (detail_tx, detail_rx) = detail_channel();
    // One control shared by the sampler; a soak run never changes it, but building
    // it the way the real program does is the point of a soak run.
    let sampling = SamplingControl::new(
        config.sample_interval,
        monitrs_core::diagnostics::Thresholds::default(),
    );
    match config.source {
        SourceKind::Fake => {
            let scenario = Scenario::with_process_count(config.processes);
            spawn_sampler_thread(
                &mut workers,
                FakeCollector::new(scenario.clone()).with_interval(config.sample_interval),
                sender.clone(),
                shutdown.clone(),
                sampling.clone(),
                SampleRequest::new(),
            )
            .expect("the sampler thread must spawn");
            spawn_detail_worker(
                &mut workers,
                FakeCollector::new(scenario),
                detail_rx,
                sender.clone(),
                shutdown.clone(),
            )
            .expect("the detail worker must spawn");
        }
        SourceKind::Real => {
            // Two instances, because `process_detail` takes `&mut self` and the
            // detail worker must not be able to delay sampling (§8.6). Each keeps
            // its own baselines, which is why a collector is long-lived (§9.1).
            spawn_sampler_thread(
                &mut workers,
                platform_collector().expect("the platform collector must construct"),
                sender.clone(),
                shutdown.clone(),
                sampling.clone(),
                SampleRequest::new(),
            )
            .expect("the sampler thread must spawn");
            spawn_detail_worker(
                &mut workers,
                platform_collector().expect("the platform collector must construct"),
                detail_rx,
                sender.clone(),
                shutdown.clone(),
            )
            .expect("the detail worker must spawn");
        }
    }

    spawn_tick_thread(
        &mut workers,
        sender.clone(),
        shutdown.clone(),
        // Fast enough to exercise the §6.2 sequence timeout, slow enough that ticks
        // do not dominate the channel.
        Duration::from_millis(100),
    )
    .expect("the tick thread must spawn");

    let (ack_tx, ack_rx) = crossbeam_channel::bounded::<Instant>(1);
    let injector = Injector::new();
    spawn_input_injector(
        &mut workers,
        sender.clone(),
        ack_rx,
        input_stop.clone(),
        &injector,
    )
    .expect("the input injector must spawn");
    let spawned = workers.len();

    // A wide terminal so the reducer builds a full table rather than a compact one.
    let mut state = AppState::new(AppSettings {
        started_at,
        size: (160, 48),
        history: config.history,
        sample_interval: config.sample_interval,
        ..AppSettings::default()
    });

    // Against the fake collector, a process the reference scenario always contains,
    // so the worker answers with a loaded detail rather than "vanished". Against
    // the real one, the target is not known until a snapshot names us: our own
    // process is the one certain to still exist by the time the read happens.
    let mut detail_target = match config.source {
        SourceKind::Fake => Some(ProcessIdentity::new(31_842, 900_100)),
        SourceKind::Real => None,
    };

    let mut measurements: Vec<Measurement> = Vec::new();
    let mut snapshots: u64 = 0;
    let mut details: u64 = 0;
    let mut last_sequence: u64 = 0;
    let mut peak_queue: usize = 0;
    let mut warm_up = Duration::ZERO;
    let mut measuring_from: Option<Instant> = None;
    let mut next_measurement = Instant::now();

    let deadline = started_at + config.duration;
    let warm_up_deadline = started_at + config.warm_up_cap;
    // The run may overshoot its requested length to finish collecting a readable
    // series, but not indefinitely: past this, the assertions report what was
    // actually gathered rather than waiting for more.
    let hard_stop = warm_up_deadline + config.duration + config.measure_every * TARGET_MEASUREMENTS;

    while Instant::now() < deadline
        || (measurements.len() < MIN_MEASUREMENTS && Instant::now() < hard_stop)
    {
        peak_queue = peak_queue.max(receiver.len());
        assert!(
            receiver.len() <= EVENT_CHANNEL_CAPACITY,
            "§16.2: the event channel grew past its bound: {} > {EVENT_CHANNEL_CAPACITY}",
            receiver.len()
        );

        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(SoakEvent::Snapshot(snapshot)) => {
                // Exactly what the event loop does: take the newest, keep the rest.
                let (newest, others) = drain_to_newest_snapshot(&receiver, snapshot, &health);
                assert!(
                    newest.sequence >= last_sequence,
                    "§8.1: snapshot sequence went backwards: {} after {last_sequence}",
                    newest.sequence
                );
                if snapshots > 0 {
                    assert!(
                        newest.sequence > last_sequence,
                        "a re-delivered sequence {} must not reach the reducer",
                        newest.sequence
                    );
                }
                last_sequence = newest.sequence;
                snapshots += 1;
                if detail_target.is_none() {
                    detail_target = newest
                        .process_by_pid(std::process::id())
                        .map(|process| process.identity);
                }
                absorb(
                    &mut state,
                    SoakEvent::Snapshot(newest),
                    &ack_tx,
                    &mut details,
                );

                for event in others {
                    absorb(&mut state, event, &ack_tx, &mut details);
                }

                // Ask for detail periodically. `try_send` because the request
                // channel is deliberately shallow: a queue of stale requests would
                // describe rows the selection has already left (§10.3).
                if let Some(target) = detail_target
                    && snapshots.is_multiple_of(16)
                {
                    let _ = detail_tx.try_send(DetailRequest::Fetch(target));
                }
            }
            Ok(event) => absorb(&mut state, event, &ack_tx, &mut details),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        // Phase boundary: the ring is full, so retained history has stopped
        // growing and a resident-size trend means something.
        if measuring_from.is_none() {
            let full = state.history().len() >= state.history().capacity();
            if full || Instant::now() >= warm_up_deadline {
                warm_up = started_at.elapsed();
                let now = Instant::now();
                measuring_from = Some(now);
                next_measurement = now;
            }
        }

        if let Some(from) = measuring_from
            && Instant::now() >= next_measurement
        {
            let usage = SelfUsage::sample();
            measurements.push(Measurement {
                at: from.elapsed(),
                resident_bytes: usage.resident_bytes.fresh().copied().unwrap_or(0),
                descriptors: usage.open_descriptors.fresh().copied().unwrap_or(0),
                snapshots_seen: snapshots,
                history_bytes: state.history().estimated_bytes(),
            });
            next_measurement += config.measure_every;
        }
    }

    // The injector stops first, and the UI keeps draining until it has actually
    // left its loop. A keypress still in flight when the stall begins would sit in
    // a full channel for its whole send timeout and be counted as *dropped* — a
    // different failure from the one the stall probe is looking for.
    input_stop.trigger();
    let wind_down = Instant::now() + ACK_TIMEOUT + Duration::from_secs(1);
    while !injector.finished.load(Ordering::Acquire) && Instant::now() < wind_down {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => absorb(&mut state, event, &ack_tx, &mut details),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    // And keep draining a little longer, for the same reason applied to the detail
    // worker: a detail reply is not coalescable either, and one computed just before
    // the stall would otherwise sit in a full channel for its whole send timeout.
    // The worker polls its request queue every 100 ms, so this covers two polls.
    let flush = Instant::now() + Duration::from_millis(300);
    while Instant::now() < flush {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => absorb(&mut state, event, &ack_tx, &mut details),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    // §16.2, deterministically: with the UI not draining, the channel must reach
    // its bound and stay there while the surplus is *counted*.
    let coalesced_before_stall = health.coalesced();
    let stall_deadline = Instant::now() + Duration::from_secs(10);
    while receiver.len() < EVENT_CHANNEL_CAPACITY && Instant::now() < stall_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // Once full, give the sampler a few more intervals to be turned away.
    std::thread::sleep(config.sample_interval * 4 + Duration::from_millis(100));
    let queue_after_stall = receiver.len();
    let coalesced_after_stall = health.coalesced();

    shutdown.trigger();
    // Drain while the workers wind down so a worker blocked on a full channel can
    // still finish, and so an in-flight keypress is acknowledged rather than left
    // to time out.
    let drain_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < drain_deadline {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(SoakEvent::Terminal(_)) => {
                let _ = ack_tx.try_send(Instant::now());
            }
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    let history = (state.history().len(), state.history().capacity());
    let history_ceiling = state.history().limits().estimated_capacity_bytes();
    let failed = workers.join_all();
    let latencies = injector
        .latencies
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();

    SoakReport {
        config,
        elapsed: started_at.elapsed(),
        warm_up,
        measurements,
        snapshots,
        last_sequence,
        details,
        latencies,
        unanswered: injector.unanswered.load(Ordering::Relaxed),
        peak_queue,
        coalesced: health.coalesced(),
        dropped: health.dropped(),
        coalesced_before_stall,
        coalesced_after_stall,
        queue_after_stall,
        history,
        history_ceiling,
        workers: (spawned, failed),
    }
}

// -------------------------------------------------------------------- the tests

/// The soak run itself.
///
/// `#[ignore]` because even the default ten seconds is too slow for `cargo test`,
/// and because CI runs the ignored set separately (see `.github/workflows/ci.yml`),
/// which means this harness is exercised at its default length on both test runners
/// — Linux x86_64 and macOS arm64 — without ever holding up a pull request.
#[test]
#[ignore = "soak run: seconds by default, hours on request. See docs/soak-testing.md"]
fn the_runtime_neither_grows_nor_stalls_under_sustained_load() {
    let config = SoakConfig::from_env();
    let report = run_soak(config);

    // Printed before the assertions so that a failure is diagnosable from the
    // series rather than from a single number.
    println!("{report}");

    // ---- the workers ----
    assert_eq!(report.workers.0, 4, "sampler, tick, detail, and injector");
    assert!(
        report.workers.1.is_empty(),
        "§10.3: every worker must join on shutdown; these did not: {:?}",
        report.workers.1
    );

    // ---- the sampler kept producing, in order, throughout ----
    assert!(
        report.snapshots > 4,
        "the sampler produced almost nothing: {} snapshots in {:?}",
        report.snapshots,
        report.elapsed
    );
    assert!(
        report.last_sequence >= report.snapshots - 1,
        "sequences must advance at least as fast as snapshots are drawn: {} vs {}",
        report.last_sequence,
        report.snapshots
    );
    assert!(
        report.details > 0,
        "the detail worker never answered, so its descriptors were never exercised"
    );

    // ---- the channel stayed bounded, and its losses were counted ----
    assert!(
        report.peak_queue <= EVENT_CHANNEL_CAPACITY,
        "§16.2: the channel grew past its bound: {} > {EVENT_CHANNEL_CAPACITY}",
        report.peak_queue
    );
    assert_eq!(
        report.dropped, 0,
        "§10.3: a keypress must never be silently lost, but {} were",
        report.dropped
    );
    assert!(
        report.queue_after_stall <= EVENT_CHANNEL_CAPACITY,
        "§16.2: a stalled UI must not grow the queue: {} > {EVENT_CHANNEL_CAPACITY}",
        report.queue_after_stall
    );
    assert!(
        report.coalesced_after_stall > report.coalesced_before_stall,
        "§16.2: a stalled UI must coalesce, and the loss must be counted rather \
         than silent: the counter stayed at {}",
        report.coalesced_before_stall
    );

    // ---- input stayed responsive ----
    assert!(
        !report.latencies.is_empty(),
        "no keypress was acknowledged, so responsiveness was not measured"
    );
    assert!(
        report.unanswered <= 1,
        "§16.2: {} keypresses went unanswered; at most the one in flight at \
         shutdown is acceptable",
        report.unanswered
    );
    let median = report.median_latency();
    assert!(
        median <= MEDIAN_LATENCY_CEILING,
        "§16.2: median input latency {median:?} exceeds {MEDIAN_LATENCY_CEILING:?} \
         under load"
    );
    let worst = report.max_latency();
    assert!(
        worst <= MAX_LATENCY_CEILING,
        "§16.2: worst input latency {worst:?} exceeds {MAX_LATENCY_CEILING:?}, which \
         is a stall rather than slowness"
    );

    // ---- history stayed inside its own budget ----
    assert_eq!(
        report.history.0, report.history.1,
        "the history ring did not fill, so no trend can be read from this run; \
         raise {ENV_SECONDS}"
    );
    for point in &report.measurements {
        assert!(
            point.history_bytes <= report.history_ceiling,
            "§8.5: retained history {} B exceeds the ring's own worst case {} B",
            point.history_bytes,
            report.history_ceiling
        );
    }

    // ---- the footprint did not trend upward ----
    if !SELF_MEASUREMENT_COMPILED {
        // §26: unavailable is not zero. Do not pretend the trend was flat when it
        // was never measured.
        println!(
            "self-measurement is not implemented for this build, so the §16.1 \
             memory and descriptor budgets were NOT verified by this run"
        );
        return;
    }

    assert!(
        report.measurements.len() >= MIN_MEASUREMENTS,
        "only {} measurements: too few to read a trend from. Raise {ENV_SECONDS}.",
        report.measurements.len()
    );

    let mut seen = 0;
    for point in &report.measurements {
        assert!(
            point.snapshots_seen > seen,
            "the sampler stopped producing between measurements: still {} at {:?}",
            point.snapshots_seen,
            point.at
        );
        seen = point.snapshots_seen;
        assert!(
            point.resident_bytes > 0,
            "a compiled-in measurement returned nothing at {:?}",
            point.at
        );
        assert!(
            point.descriptors > 0,
            "a compiled-in descriptor count returned nothing at {:?}",
            point.at
        );
    }

    let (first, last) = report.quartiles();
    let first_resident = SoakReport::mean_resident(first);
    let last_resident = SoakReport::mean_resident(last);
    let allowance = (first_resident / 4).max(RESIDENT_TOLERANCE_BYTES);
    assert!(
        last_resident <= first_resident.saturating_add(allowance),
        "§16.1: resident memory trended upward: {} KiB in the first quartile, \
         {} KiB in the last, allowance {} KiB",
        first_resident / 1024,
        last_resident / 1024,
        allowance / 1024
    );

    let first_fds = SoakReport::peak_descriptors(first);
    let last_fds = SoakReport::peak_descriptors(last);
    assert!(
        last_fds <= first_fds.saturating_add(DESCRIPTOR_TOLERANCE),
        "§16.1: file descriptors trended upward: peak {first_fds} in the first \
         quartile, {last_fds} in the last"
    );
}

/// The configuration knobs must behave, and that is worth checking without a soak.
#[test]
fn the_default_configuration_is_a_smoke_test_not_a_twelve_hour_run() {
    // Guard against a default that would make CI's `--ignored` pass unusable.
    let config = SoakConfig::from_env();
    if std::env::var(ENV_SECONDS).is_ok() {
        return;
    }
    assert!(
        config.duration <= Duration::from_secs(30),
        "the default run must stay a smoke test, got {:?}",
        config.duration
    );
    assert!(
        config.measure_every >= MIN_MEASURE_EVERY,
        "measuring more often than {MIN_MEASURE_EVERY:?} would perturb what it measures"
    );
    assert_eq!(
        config.source,
        SourceKind::Fake,
        "the default run must be reproducible, so it drives the fake collector"
    );
    assert!(
        config.warm_up_cap >= WARM_UP_FLOOR,
        "a short run on a slow machine must be allowed to overshoot its warm-up \
         rather than fail for want of a full history ring"
    );
    let fits = u32::try_from(MIN_MEASUREMENTS).unwrap_or(u32::MAX);
    assert!(
        config.measure_every * fits <= config.duration,
        "the default run must fit at least {MIN_MEASUREMENTS} measurements"
    );
}

/// A long run must be measured on the shipped history configuration, not on the
/// small ring a short smoke run needs.
#[test]
fn a_long_run_soaks_the_shipped_history_configuration() {
    let short = HistoryConfig {
        interval: MIN_SAMPLE_INTERVAL,
        duration: MIN_HISTORY_DURATION,
        ..HistoryConfig::default()
    };
    // Constructed directly rather than through the environment so the test does not
    // depend on process-wide state another test could be setting.
    assert_eq!(
        HistoryConfig::default().duration,
        Duration::from_secs(300),
        "§8.5's shipped history span"
    );
    assert!(
        SHIPPED_HISTORY_FROM >= HistoryConfig::default().duration,
        "a run shorter than the ring's own span cannot fill it and then show a trend"
    );
    assert_ne!(
        short,
        HistoryConfig::default(),
        "the smoke-run ring must actually be smaller than the shipped one"
    );
}
