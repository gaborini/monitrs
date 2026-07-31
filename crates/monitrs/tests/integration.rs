//! The §17.5 integration tests: the assembled application, end to end.
//!
//! Every other test suite in the workspace proves one layer. These prove the
//! *seams* — the nine behaviours §17.5 names, each of which crosses at least two
//! of them:
//!
//! ```text
//!   FakeCollector ──> runtime.rs workers ──> bounded channel ──> apply() ──> AppState ──> views::render
//!                     (real threads)         (real coalescing)   (real reducer)          (TestBackend)
//! ```
//!
//! Nothing here is a mock, and nothing here builds a `SystemSnapshot` by hand.
//! The stream comes from [`FakeCollector`] driven through [`SampleTick`] exactly
//! the way `spawn_sampler_thread` drives it, including the [`PressureEngine`] pass
//! the sampler performs before publishing (§10.4: the UI never sees a snapshot
//! whose radar disagrees with its own metrics). A scenario knob — `exiting_at`,
//! `reused_as`, `fail_at`, `collect_delay`, `with_process_count` — is how a
//! condition is set up, because a hand-built snapshot can be made to say anything,
//! including something the real collector would never produce.
//!
//! # Where the real threads are used, and where they are not
//!
//! Two of the nine behaviours are genuinely concurrent — snapshot coalescing and
//! worker shutdown — and those drive the real workers from `src/runtime.rs`. The
//! other seven are decisions, and a decision belongs to the reducer: they go
//! through `monitrs_tui::app::apply` on a single thread, where a failure is a
//! diagnosis rather than a race.
//!
//! # Why the module source is included rather than imported
//!
//! `monitrs` is a binary crate, so there is no library target to link against and
//! `runtime.rs`, `config.rs` and `export.rs` are deliberately `pub(crate)`.
//! `#[path]` puts the real modules into this test binary, which is the only way to
//! drive them without widening their visibility for the sake of a test.
//! `crates/monitrs/tests/soak.rs` does the same and explains the trade-off it
//! carries: `cargo test` sets `cfg(test)` for an integration target too, so each
//! included module's own unit tests are compiled into this binary and run again
//! here under `runtime::tests::*`, `config::tests::*` and `export::tests::*`.
//! `cli` is included only because `config` needs it for `--flag` precedence.
//!
//! # Two ways an integration test rots, both guarded against here
//!
//! * **Host dependence.** Nothing below reads the architecture, the hostname, the
//!   real clock or the real process table. The fake host is `dev-mbp` on
//!   `aarch64` on every runner, wall-clock times are offsets from the Unix epoch,
//!   and frames are rendered in strict-ASCII so a developer's locale cannot change
//!   what the assertions see.
//! * **Fixed sleeps.** A `sleep` long enough for a loaded CI runner is slow on
//!   every other run and still a guess. The concurrent tests wait on a
//!   *condition* with a generous ceiling ([`wait_until`]) instead.
//!
//! Appearance is not asserted here. §17.3's snapshot suites own that; these tests
//! assert properties of a frame — that it renders at all, that it says `warming
//! up` rather than `0`, that the timeline badge changed — so that a legitimate
//! cosmetic change breaks one suite rather than two.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              failure must name the line that broke"
)]

use core::time::Duration;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use monitrs_collectors::fake::{FakeProcess, Pattern, Scenario};
use monitrs_collectors::{DueTiers, FakeCollector, SampleTick, SnapshotSource as _};
use monitrs_core::SystemSnapshot;
use monitrs_core::diagnostics::{PressureEngine, Thresholds};
use monitrs_core::history::HistoryConfig;
use monitrs_core::model::{MetricState, ProcessIdentity};
use monitrs_core::process::{ProcessSort, ProcessSortKey};
use monitrs_tui::action::{Effect, Effects};
use monitrs_tui::app::{
    AppSettings, AppState, Notice, NoticeKind, OverlayKind, PanelFocus, TimelineStatus, apply,
};
use monitrs_tui::event::{Event, Key, KeyPress, TerminalEvent};
use monitrs_tui::glyphs::GlyphSet;
use monitrs_tui::layout::Breakpoint;
use monitrs_tui::theme::{ColorDepth, ThemeId};
use monitrs_tui::views;
use monitrs_tui::widgets::Presentation;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

#[path = "../src/cli.rs"]
mod cli;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/export.rs"]
mod export;
// `runtime` records collector timings through `logging`, so the real module has to
// come along: a stub would let the test pass while the log the runtime writes went
// untested (§14.2).
#[path = "../src/logging.rs"]
mod logging;
#[path = "../src/runtime.rs"]
mod runtime;

use config::Config;
use export::{RedactionPolicy, SnapshotExport};
use runtime::{
    EVENT_CHANNEL_CAPACITY, SampleRequest, SamplingControl, SensorInterest, Shutdown, Workers,
    detail_channel, drain_to_newest_snapshot, event_channel, spawn_detail_worker,
    spawn_sampler_thread, spawn_tick_thread,
};

/// The event payload type for the tests that never reload configuration.
type TestEvent = Event<()>;

/// The `Event::ConfigReloaded` payload, spelled as `interactive.rs` spells it.
///
/// §10.2's reload variant carries `Result<Config, ConfigError>`; the reducer is
/// generic over it so that `monitrs-tui` never depends on the binary's config
/// types (§10.1). Using the binary's real payload here is the point of the test:
/// a reload that only type-checks against `()` would prove nothing.
type ConfigOutcome = Result<Box<Config>, String>;

/// A terminal wide enough for §5.7's `Wide` band, where every panel is drawn.
const WIDE: (u16, u16) = (160, 48);

/// Longest a concurrent test waits for a worker thread to reach a state.
///
/// Generous on purpose: this bounds a *failure*, not the normal path, so a loaded
/// runner should never reach it and a hung worker should never hang the suite.
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often [`wait_until`] re-checks. Short enough not to add latency of its own.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Attempts [`Feed::next`] makes before concluding the scenario cannot produce a
/// snapshot at all.
///
/// A `fail_at` scenario fails one sequence, so two attempts always suffice; the
/// ceiling exists so a misconfigured scenario fails the test instead of looping.
const MAX_FEED_ATTEMPTS: u32 = 8;

// ---------------------------------------------------------------- the harness

/// Blocks until `condition` holds, and reports whether it ever did.
///
/// The alternative — sleeping for a fixed span and then asserting — is flaky on a
/// loaded runner in one direction and slow in the other.
fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    condition()
}

/// A deterministic snapshot stream, assembled the way the sampler thread does.
///
/// The tick's `captured_at` advances by a fixed nominal interval and its
/// `wall_time` by one second from the Unix epoch, so nothing in a produced
/// snapshot depends on when or where the test runs.
#[derive(Debug)]
struct Feed {
    collector: FakeCollector,
    /// Owned here rather than by the collector: §2.3 makes pressure a policy
    /// decision over snapshot plus history, which is why the sampler — not the
    /// collector — fills it in.
    pressure: PressureEngine,
    origin: Instant,
    interval: Duration,
    tick: SampleTick,
    /// Whether [`Feed::next`] has produced anything yet, so sequence 0 is emitted
    /// from `SampleTick::first` rather than from an advance.
    started: bool,
}

impl Feed {
    /// A feed over `scenario` at the default one-second nominal interval.
    fn new(scenario: Scenario) -> Self {
        let origin = Instant::now();
        let interval = Duration::from_secs(1);
        Self {
            collector: FakeCollector::new(scenario).with_interval(interval),
            pressure: PressureEngine::new(Thresholds::default()),
            origin,
            interval,
            tick: SampleTick::first(origin, SystemTime::UNIX_EPOCH),
            started: false,
        }
    }

    /// The next snapshot, published as the sampler publishes it.
    ///
    /// A scenario-injected failure consumes its sequence and is retried at the
    /// next one, which is exactly what `spawn_sampler_thread` does and why §8.1's
    /// sequence identifies the *attempt* rather than the sample.
    fn next(&mut self) -> Arc<SystemSnapshot> {
        for _ in 0..MAX_FEED_ATTEMPTS {
            if self.started {
                let sequence = self.tick.sequence.saturating_add(1);
                let steps = u32::try_from(sequence).unwrap_or(u32::MAX);
                self.tick = self.tick.advance(
                    self.origin + self.interval.saturating_mul(steps),
                    SystemTime::UNIX_EPOCH + Duration::from_secs(sequence),
                    DueTiers::ALL,
                );
            }
            self.started = true;
            if let Ok(mut snapshot) = self.collector.sample(&self.tick) {
                snapshot.pressure = self.pressure.observe(&snapshot);
                return Arc::new(snapshot);
            }
        }
        panic!("the scenario refused {MAX_FEED_ATTEMPTS} consecutive samples");
    }
}

/// A reducer, a snapshot stream, and the keystrokes that connect them.
///
/// Every mutation of the state below goes through [`apply`], so no test can put
/// the state into a shape the running program could not reach.
#[derive(Debug)]
struct Harness {
    feed: Feed,
    state: AppState,
}

impl Harness {
    /// A harness over `scenario` at `size`, sorted so the table order is fixed.
    ///
    /// The ordering is pinned rather than defaulted because a test that selects
    /// "the third row" must mean the same row on every run.
    fn new(scenario: Scenario, size: (u16, u16)) -> Self {
        Self::with_settings(scenario, size, |_| {})
    }

    /// As [`Harness::new`], with a chance to adjust the settings first.
    fn with_settings(
        scenario: Scenario,
        size: (u16, u16),
        adjust: impl FnOnce(&mut AppSettings),
    ) -> Self {
        let feed = Feed::new(scenario);
        let mut settings = AppSettings {
            started_at: feed.origin,
            size,
            sort: ProcessSort::descending(ProcessSortKey::Cpu),
            history: HistoryConfig::default(),
            sample_interval: feed.interval,
            ..AppSettings::default()
        };
        adjust(&mut settings);
        let state = AppState::new(settings);
        Self { feed, state }
    }

    /// Feeds one snapshot through the reducer and returns its effects.
    fn sample(&mut self) -> Effects {
        let snapshot = self.feed.next();
        apply(&mut self.state, TestEvent::Snapshot(snapshot))
    }

    /// Feeds `count` snapshots, drawing a frame after each.
    ///
    /// Drawing matters: `record_render` is what clears the "not shown yet" flag,
    /// and without it every snapshot after the first would be counted as coalesced
    /// (§10.3) and the run would not resemble a running program at all.
    fn run(&mut self, count: usize) {
        for _ in 0..count {
            let _ = self.sample();
            self.draw();
        }
    }

    /// Renders one frame and records it, as the event loop does.
    fn draw(&mut self) -> String {
        let text = frame_text(&self.state);
        let at = self.state.clock() + Duration::from_millis(4);
        self.state.record_render(at, Duration::from_millis(4));
        text
    }

    /// Presses one character key.
    fn press(&mut self, key: char) -> Effects {
        apply::<()>(&mut self.state, Event::key(KeyPress::char(key)))
    }

    /// Presses one non-character key.
    fn press_key(&mut self, key: Key) -> Effects {
        apply::<()>(&mut self.state, Event::key(KeyPress::plain(key)))
    }

    /// Types `text` one key at a time, as a user would.
    fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            let _ = self.press(character);
        }
    }

    /// Delivers a resize event.
    fn resize(&mut self, columns: u16, rows: u16) -> Effects {
        apply::<()>(
            &mut self.state,
            Event::Terminal(TerminalEvent::Resize { columns, rows }),
        )
    }

    /// Moves the selection onto `identity` using only bound keys.
    ///
    /// `Home` then `j`, rather than reaching into the selection, so the test
    /// exercises the same path the user does and fails if the binding changes.
    fn select(&mut self, identity: ProcessIdentity) {
        let _ = self.press_key(Key::Home);
        for _ in 0..=self.state.rows().len() {
            if self.state.selected() == Some(identity) {
                return;
            }
            let _ = self.press('j');
        }
        panic!(
            "could not reach {identity:?} with {} rows visible",
            self.state.rows().len()
        );
    }

    /// The sequence of the snapshot on screen.
    fn displayed_sequence(&self) -> Option<u64> {
        self.state.snapshot().map(|snapshot| snapshot.sequence)
    }

    /// The sequence of the newest snapshot received.
    fn live_sequence(&self) -> Option<u64> {
        self.state.live_snapshot().map(|snapshot| snapshot.sequence)
    }

    /// The most recent notice, for asserting *which* refusal happened.
    fn latest_notice(&self) -> Notice {
        self.state
            .notice_log()
            .latest()
            .cloned()
            .expect("something should have been reported")
    }
}

/// Strict ASCII, default theme, full colour.
///
/// ASCII rather than the resolved glyph set: §5.1 requires the whole interface to
/// work in 7-bit output, and asserting on ASCII means a runner's locale cannot
/// change what a frame contains.
fn presentation() -> Presentation<'static> {
    Presentation::new(
        GlyphSet::ascii(),
        ThemeId::DefaultDark.theme(),
        ColorDepth::TrueColor,
    )
}

/// Renders one whole frame of `state` and returns its characters, one line per row.
///
/// A zero-width or zero-height state still renders: the backend is clamped to one
/// cell while the drawn area keeps the real size, so §5.7's rule that a zero-area
/// rect must not panic is exercised rather than side-stepped.
fn frame_text(state: &AppState) -> String {
    let (columns, rows) = state.size();
    let mut terminal = Terminal::new(TestBackend::new(columns.max(1), rows.max(1)))
        .expect("a test backend never fails to initialise");
    terminal
        .draw(|frame| {
            views::render(frame, Rect::new(0, 0, columns, rows), state, presentation());
        })
        .expect("drawing to a test backend never fails");

    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in buffer.area.top()..buffer.area.bottom() {
        for x in buffer.area.left()..buffer.area.right() {
            if let Some(cell) = buffer.cell((x, y)) {
                text.push_str(cell.symbol());
            }
        }
        text.push('\n');
    }
    text
}

/// Asserts that `state` is warming up, and that nothing can read a value from it.
///
/// Both halves matter. §8.2 and §26 forbid the first delta sample from being zero,
/// and `fresh()` returning `None` is what stops a caller from quietly treating the
/// placeholder as a measurement.
fn assert_warming_up<T: core::fmt::Debug>(label: &str, state: &MetricState<T>) {
    assert!(
        state.is_warming_up(),
        "{label} should be warming up on the first sample, got {state:?}"
    );
    assert!(
        state.fresh().is_none(),
        "{label} must not offer a value while warming up"
    );
}

/// A directory that removes itself, so no test leaves files behind.
#[derive(Debug)]
struct ScratchDir(PathBuf);

impl ScratchDir {
    /// Creates an empty directory named after `label` and this process.
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "monitrs-integration-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create the scratch directory");
        Self(base)
    }

    /// Writes `contents` to `name` and returns its path.
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write a fixture file");
        path
    }

    /// A path inside the directory, which need not exist.
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ------------------------------------------------------ startup to first frame

#[test]
fn the_first_snapshot_reaches_the_screen_with_every_delta_metric_warming_up() {
    let mut harness = Harness::new(Scenario::default(), WIDE);

    let effects = harness.sample();

    assert!(
        effects.contains(&Effect::RequestRedraw),
        "the first snapshot must ask for a frame, got {effects:?}"
    );
    assert!(
        harness.state.has_unrendered_snapshot(),
        "§10.3: the newest snapshot is pending until a frame is recorded"
    );
    let snapshot = harness
        .state
        .snapshot()
        .expect("the first snapshot is on screen")
        .clone();
    assert_eq!(snapshot.sequence, 0);
    assert_eq!(
        snapshot.elapsed,
        Duration::ZERO,
        "§8.1: the first sample has no measured interval"
    );

    // §26: unavailable is never zero. Every delta-derived field, not a sample of
    // them — the first frame is exactly where a `0` slips through unnoticed.
    assert_warming_up("cpu.total", &snapshot.cpu.total);
    assert_warming_up("cpu.per_core", &snapshot.cpu.per_core);
    assert_warming_up("load", &snapshot.load);
    for process in &snapshot.processes {
        assert_warming_up(&format!("{} cpu", process.name), &process.cpu);
        assert_warming_up(&format!("{} io.read", process.name), &process.io.read);
        assert_warming_up(&format!("{} io.write", process.name), &process.io.write);
    }
    for disk in &snapshot.disks {
        assert_warming_up(&format!("{} read", disk.device), &disk.read);
        assert_warming_up(&format!("{} write", disk.device), &disk.write);
    }
    for interface in &snapshot.networks {
        assert_warming_up(&format!("{} rx", interface.name), &interface.rx);
        assert_warming_up(&format!("{} tx", interface.name), &interface.tx);
    }

    let frame = harness.draw();

    assert!(
        frame.contains("warming up"),
        "the first frame must say so in words:\n{frame}"
    );
    assert!(
        frame.contains("LIVE"),
        "§2.1: the header must state the timeline state:\n{frame}"
    );
    assert!(
        frame.contains("rustc"),
        "the process table is populated from the first sample:\n{frame}"
    );
    assert!(
        !harness.state.has_unrendered_snapshot(),
        "recording the frame clears the pending flag"
    );
    assert_eq!(harness.state.render_timing().frames(), 1);
    assert_eq!(
        harness.state.health().coalesced_samples,
        0,
        "§10.3: a snapshot that was drawn was not coalesced"
    );
}

#[test]
fn the_frame_after_the_first_shows_measured_values_instead_of_warming_up() {
    let mut harness = Harness::new(Scenario::default(), WIDE);

    harness.run(2);

    let snapshot = harness
        .state
        .snapshot()
        .expect("two snapshots have arrived")
        .clone();
    assert_eq!(snapshot.sequence, 1);
    assert!(
        snapshot.elapsed > Duration::ZERO,
        "§8.1: the second sample carries a measured interval"
    );
    assert!(
        snapshot.cpu.total.fresh().is_some(),
        "warming up must be a state the pipeline leaves, not one it stays in"
    );
    assert_eq!(
        harness.displayed_sequence(),
        harness.live_sequence(),
        "a live timeline shows the newest sample"
    );
    assert_eq!(harness.state.history().len(), 2, "§8.5: both were recorded");
}

// ------------------------------------------------------------ terminal resize

#[test]
fn a_resize_walks_the_layout_down_to_too_small_and_back_without_losing_the_focus() {
    let mut harness = Harness::new(Scenario::default(), WIDE);
    harness.run(3);
    assert_eq!(harness.state.layout().breakpoint, Breakpoint::Wide);
    assert_eq!(harness.state.focus(), PanelFocus::Processes);

    // Down through every band §5.7 defines, then back to the top.
    let bands = [
        (160, 48, Breakpoint::Wide),
        (120, 30, Breakpoint::Standard),
        (80, 24, Breakpoint::Compact),
        (70, 18, Breakpoint::TooSmall),
        (40, 10, Breakpoint::TooSmall),
        (80, 24, Breakpoint::Compact),
        (160, 48, Breakpoint::Wide),
    ];

    for (columns, rows, expected) in bands {
        let effects = harness.resize(columns, rows);

        assert!(
            effects.contains(&Effect::RequestRedraw),
            "a resize must repaint at {columns}x{rows}"
        );
        assert_eq!(harness.state.size(), (columns, rows));
        let layout = harness.state.layout();
        assert_eq!(
            layout.breakpoint, expected,
            "{columns}x{rows} should be {expected:?}"
        );

        let area = harness.state.area();
        for (name, panel) in layout.named_panels() {
            let Some(panel) = panel else { continue };
            assert!(
                panel.right() <= area.right() && panel.bottom() <= area.bottom(),
                "§5.7: the {name} panel {panel:?} left the terminal {area:?}"
            );
        }
        if layout.named_panels().iter().any(|(_, rect)| rect.is_some()) && !layout.shows_notice() {
            assert!(
                harness.state.focus().is_present(&layout),
                "§5.7: focus stayed on a panel {expected:?} does not draw"
            );
        }
        assert!(
            harness.state.page_size() >= 1,
            "a page jump must move at least one row at {columns}x{rows}"
        );

        let frame = harness.draw();
        assert_eq!(
            frame.lines().count(),
            usize::from(rows.max(1)),
            "the frame must fill the terminal at {columns}x{rows}"
        );
    }

    // The two ends of the too-small band render different things, and §5.7 is
    // specific about both.
    let _ = harness.resize(70, 18);
    let minimal = harness.draw();
    assert!(
        minimal.contains("rustc"),
        "§5.7: 70x18 still gets a minimal process list:\n{minimal}"
    );

    let _ = harness.resize(40, 10);
    let notice = harness.draw();
    assert!(
        notice.contains("monitrs needs at least 60x16"),
        "§5.7: below 60x16 the resize notice replaces the interface:\n{notice}"
    );

    // A terminal can report zero cells mid-resize; §5.7 forbids a panic there.
    let _ = harness.resize(0, 0);
    assert_eq!(harness.state.layout().breakpoint, Breakpoint::TooSmall);
    assert_eq!(harness.state.page_size(), 1);
    let _ = harness.draw();

    let _ = harness.resize(WIDE.0, WIDE.1);
    assert_eq!(harness.state.layout().breakpoint, Breakpoint::Wide);
    assert!(
        harness.state.focus().is_present(&harness.state.layout()),
        "focus must be usable again once the terminal has room"
    );
    let restored = harness.draw();
    assert!(
        restored.contains("PROCESSES"),
        "the dashboard comes back after a resize:\n{restored}"
    );
}

// --------------------------------------------------------- snapshot coalescing

#[test]
fn snapshots_coalesce_at_the_channel_bound_and_the_newest_one_wins() {
    let (sender, receiver) = event_channel::<()>();
    let health = sender.health();
    let shutdown = Shutdown::new();
    let mut workers = Workers::new();

    // Only the sampler, so every queued event is a snapshot and the sequences in
    // the channel are consecutive. A tick thread here would make the arithmetic
    // below depend on thread timing for no gain.
    spawn_sampler_thread(
        &mut workers,
        FakeCollector::new(Scenario::default()),
        sender,
        shutdown.clone(),
        SamplingControl::new(Duration::from_millis(1), Thresholds::default()),
        SampleRequest::new(),
        SensorInterest::new(),
    )
    .expect("the sampler thread must spawn");

    // Nothing drains, so the channel fills to its bound and stays there.
    assert!(
        wait_until(|| receiver.len() >= EVENT_CHANNEL_CAPACITY),
        "the sampler never filled the channel: {} of {EVENT_CHANNEL_CAPACITY}",
        receiver.len()
    );
    assert!(
        wait_until(|| health.coalesced() > 0),
        "§16.2: a full channel must coalesce, and the loss must be counted"
    );
    assert_eq!(
        receiver.len(),
        EVENT_CHANNEL_CAPACITY,
        "§10.3: the queue must not grow past its bound"
    );
    assert_eq!(
        health.dropped(),
        0,
        "§10.3: a coalesced snapshot is superseded, not lost"
    );

    // Stopped before draining so that no snapshot arrives mid-drain: what is in
    // the channel now is exactly sequences 0..EVENT_CHANNEL_CAPACITY.
    shutdown.trigger();
    assert!(
        workers.join_all().is_empty(),
        "the sampler must join even with a full channel"
    );
    let coalesced_by_sender = health.coalesced();

    let first = match receiver.recv().expect("the queue is not empty") {
        TestEvent::Snapshot(snapshot) => snapshot,
        other => panic!("expected a snapshot, got {}", other.kind()),
    };
    assert_eq!(first.sequence, 0, "the oldest queued snapshot is the first");

    let (newest, others) = drain_to_newest_snapshot(&receiver, first, &health);

    // The queue held `EVENT_CHANNEL_CAPACITY` snapshots numbered from zero, so this
    // is the newest sequence that was ever admitted to it.
    let last = u64::try_from(EVENT_CHANNEL_CAPACITY)
        .unwrap_or(u64::MAX)
        .saturating_sub(1);
    assert_eq!(
        newest.sequence, last,
        "§10.3: draining must keep the newest queued snapshot"
    );
    assert!(
        others.is_empty(),
        "the sampler emits nothing but snapshots, so nothing else can survive"
    );
    assert!(
        receiver.is_empty(),
        "draining must leave no backlog of stale samples"
    );
    assert_eq!(
        health.coalesced(),
        coalesced_by_sender.saturating_add(last),
        "every superseded snapshot is counted, on the sender and in the drain"
    );

    // And the one that survived is the one the user sees.
    let mut state = AppState::new(AppSettings {
        started_at: Instant::now(),
        size: WIDE,
        ..AppSettings::default()
    });
    let effects = apply(&mut state, TestEvent::Snapshot(newest));
    assert!(effects.contains(&Effect::RequestRedraw));
    assert_eq!(
        state.snapshot().map(|snapshot| snapshot.sequence),
        Some(last),
        "the newest snapshot is the one that reaches the screen"
    );
    assert_eq!(
        state.history().len(),
        1,
        "§10.3: coalesced samples are dropped, not queued into history"
    );
}

// ------------------------------------------- filtering while snapshots arrive

#[test]
fn the_filter_and_the_selection_survive_a_stream_of_new_snapshots() {
    // A large table so the filter has real work to do, and one failing sample in
    // the middle so a recoverable collector error is part of the stream (§14.1).
    let scenario = Scenario {
        fail_at: Some(6),
        ..Scenario::with_process_count(400)
    };
    let mut harness = Harness::new(scenario, WIDE);
    harness.run(3);
    let unfiltered = harness.state.rows().len();
    assert_eq!(unfiltered, 400, "every process is visible before filtering");

    // `/`, the pattern, Enter — the same keys a user presses (§6.2).
    let _ = harness.press('/');
    assert_eq!(
        harness.state.top_overlay().map(|overlay| overlay.kind()),
        Some(OverlayKind::FilterEdit)
    );
    harness.type_text("worker");
    let _ = harness.press_key(Key::Enter);

    assert!(
        harness.state.top_overlay().is_none(),
        "submitting the filter closes the editor"
    );
    assert_eq!(harness.state.filter_text(), "worker");
    assert!(harness.state.filter().is_active());
    let filtered = harness.state.rows().len();
    assert_eq!(
        filtered, 395,
        "the five reference processes do not match `worker`"
    );

    // The thirteenth synthetic worker: `Scenario::with_process_count` numbers them
    // from PID 10,000 upwards with a start key of seven times the PID.
    harness.select(ProcessIdentity::new(10_012, 70_084));
    let watched = harness.state.selected().expect("a row is selected");
    let row = harness.state.selected_row().expect("and it has a position");
    let frame = harness.draw();
    assert!(
        !frame.contains("postgres"),
        "a filtered-out process must not be on screen:\n{frame}"
    );

    // Twelve more samples, one of which the collector refuses.
    let before = harness.live_sequence().expect("a sample has arrived");
    harness.run(12);
    let after = harness.live_sequence().expect("more samples have arrived");

    assert!(
        after > before + 12,
        "§8.1: the refused attempt consumed a sequence, so {after} should have \
         skipped one past {before}"
    );
    assert_eq!(
        harness.state.filter_text(),
        "worker",
        "§7.2: a new snapshot must not clear the filter"
    );
    assert_eq!(
        harness.state.rows().len(),
        filtered,
        "the filter still matches the same rows"
    );
    assert_eq!(
        harness.state.selected(),
        Some(watched),
        "§7.2: the selection is tracked by identity across refreshes"
    );
    assert_eq!(
        harness.state.selected_row(),
        Some(row),
        "and a stable ordering keeps it in the same visual position"
    );
    for visible in harness.state.rows().as_slice() {
        let process = harness
            .state
            .snapshot()
            .and_then(|snapshot| snapshot.process(visible.identity))
            .expect("every visible row names a process in the displayed snapshot");
        assert!(
            process.name.contains("worker"),
            "{} does not match the active filter",
            process.name
        );
    }
}

// ------------------------------------------------ pause, seek, and return live

#[test]
fn pausing_freezes_the_view_while_collection_continues_and_l_returns_to_live() {
    let mut harness = Harness::new(Scenario::default(), WIDE);
    harness.run(12);
    assert_eq!(harness.state.timeline_status(), TimelineStatus::Live);

    let frozen_at = harness.displayed_sequence().expect("a frame is on screen");
    let _ = harness.press(' ');

    assert!(
        matches!(
            harness.state.timeline_status(),
            TimelineStatus::Paused { .. }
        ),
        "§2.1: Space freezes the visible timeline, got {:?}",
        harness.state.timeline_status()
    );
    let paused = harness.draw();
    assert!(
        paused.contains("PAUSED"),
        "§2.1: the header must read PAUSED:\n{paused}"
    );

    // Collection does not stop, and the frozen view does not move.
    harness.run(5);
    assert_eq!(
        harness.displayed_sequence(),
        Some(frozen_at),
        "§2.1: a paused view keeps showing what it froze"
    );
    assert!(
        harness.live_sequence() > Some(frozen_at),
        "§2.1: pausing the display must not pause collection"
    );
    assert_eq!(
        harness.state.history().len(),
        17,
        "§8.5: the ring keeps filling while the view is frozen"
    );

    // Seeking moves into history, which reads differently again (§2.1, §26).
    let _ = harness.press('[');
    let _ = harness.press('[');
    let status = harness.state.timeline_status();
    let offset = match status {
        TimelineStatus::History { offset } => offset,
        other => panic!("§2.1: `[` must enter history, got {other:?}"),
    };
    assert!(
        offset > Duration::ZERO,
        "a historical sample must be behind live"
    );
    let history = harness.draw();
    assert!(
        history.contains("HISTORY -"),
        "§2.1: the header must show the offset:\n{history}"
    );
    assert!(
        !history.contains("[>LIVE]"),
        "§26: history must never be confused with live state:\n{history}"
    );

    let _ = harness.press('L');

    assert_eq!(
        harness.state.timeline_status(),
        TimelineStatus::Live,
        "§2.1: `L` is the one explicit return to live"
    );
    assert_eq!(
        harness.displayed_sequence(),
        harness.live_sequence(),
        "returning to live catches the display up"
    );
    let live = harness.draw();
    assert!(live.contains("LIVE"), "{live}");
    assert!(!live.contains("PAUSED"), "{live}");
}

#[test]
fn process_actions_are_refused_while_the_timeline_is_away_from_live() {
    let mut harness = Harness::new(Scenario::default(), WIDE);
    harness.run(8);
    harness.select(ProcessIdentity::new(31_842, 900_100));
    assert!(harness.state.allows_process_actions());

    for (label, away) in [("paused", ' '), ("scrubbed", '[')] {
        let _ = harness.press(away);
        assert!(
            !harness.state.allows_process_actions(),
            "§15.1: process actions must be unavailable while {label}"
        );

        // Every route to a signal: the dialog, both one-key proposals, and the two
        // confirmations. §15.1 checks the guard once, before the action is matched,
        // so a confirmation must be refused as firmly as a proposal.
        for key in ['x', 'T', 'K', 'y', 'Y'] {
            let effects = harness.press(key);

            assert!(
                !effects.touches_a_process(),
                "§15.1: `{key}` produced a process effect while {label}: {effects:?}"
            );
            assert!(
                harness.state.pending_process_action().is_none(),
                "§15.1: `{key}` left an action pending while {label}"
            );
            assert!(
                harness.state.process_action_stage().is_none(),
                "§15.1: `{key}` opened the action chain while {label}"
            );
            let notice = harness.latest_notice();
            assert_eq!(
                notice.kind,
                NoticeKind::Interaction,
                "the refusal must be reported as an interaction one, got {notice:?}"
            );
            assert!(
                notice.message.contains("not live") && notice.message.contains("press L"),
                "the refusal must explain itself and say how to undo it, got {notice:?}"
            );
        }
        let frame = harness.draw();
        assert!(
            !frame.contains("SIGKILL"),
            "no confirmation dialog may appear while {label}:\n{frame}"
        );
    }

    // Live again, and the refusal changes to one about the platform rather than
    // about the timeline: this fake system reports no signal support at all, which
    // is what keeps the assertion above about the *reason* honest.
    let _ = harness.press('L');
    assert!(harness.state.allows_process_actions());
    let effects = harness.press('x');
    assert!(!effects.touches_a_process(), "{effects:?}");
    let notice = harness.latest_notice();
    assert_eq!(notice.kind, NoticeKind::ProcessAction, "{notice:?}");
    assert!(
        notice.message.contains("not supported on this platform"),
        "a live refusal must name the real reason, got {notice:?}"
    );
}

// -------------------------------------------------------- selected process exit

#[test]
fn a_selected_process_that_exits_hands_the_selection_to_a_surviving_row() {
    let leaving = ProcessIdentity::new(2_002, 20_002);
    let scenario = Scenario {
        processes: vec![
            FakeProcess::new(1_001, 10_001, "keeper-high", "keeper --high")
                .with_cpu(Pattern::Steady(90.0)),
            FakeProcess::new(2_002, 20_002, "leaver", "leaver --doomed")
                .with_cpu(Pattern::Steady(60.0))
                .exiting_at(4),
            FakeProcess::new(3_003, 30_003, "keeper-low", "keeper --low")
                .with_cpu(Pattern::Steady(30.0)),
        ],
        ..Scenario::default()
    };
    let mut harness = Harness::new(scenario, WIDE);
    // Sequences 0 to 3: the doomed process is still there, and past warming up.
    harness.run(4);
    assert_eq!(harness.live_sequence(), Some(3));

    harness.select(leaving);
    assert_eq!(harness.state.selected_row(), Some(1), "sorted by CPU, desc");
    let before = harness.draw();
    assert!(before.contains("leaver"), "{before}");

    // Sequence 4 is the one it is missing from.
    harness.run(1);
    assert_eq!(harness.live_sequence(), Some(4));

    assert_eq!(
        harness.state.rows().len(),
        2,
        "the exited process left the table"
    );
    let selected = harness
        .state
        .selected()
        .expect("§7.2: the selection must not dangle");
    assert_ne!(selected, leaving, "the exited process is not selectable");
    assert_eq!(
        selected,
        ProcessIdentity::new(3_003, 30_003),
        "§7.2: the nearest surviving row takes over, not the top of the table"
    );
    assert_eq!(
        harness.state.selected_row(),
        Some(1),
        "§7.2: the visual position is preserved where possible"
    );
    let after = harness.draw();
    assert!(
        !after.contains("leaver"),
        "a process that exited must leave the screen:\n{after}"
    );
    assert!(after.contains("keeper-low"), "{after}");
}

#[test]
fn a_reused_pid_inherits_neither_the_selection_nor_the_pin() {
    let original = ProcessIdentity::new(2_002, 20_002);
    let recycled = ProcessIdentity::new(2_002, 99_999);
    let survivor = ProcessIdentity::new(3_003, 30_003);
    let scenario = Scenario {
        processes: vec![
            FakeProcess::new(1_001, 10_001, "keeper-high", "keeper --high")
                .with_cpu(Pattern::Steady(90.0)),
            FakeProcess::new(2_002, 20_002, "leaver", "leaver --doomed")
                .with_cpu(Pattern::Steady(60.0))
                .exiting_at(4)
                .reused_as(99_999),
            FakeProcess::new(3_003, 30_003, "keeper-low", "keeper --low")
                .with_cpu(Pattern::Steady(30.0)),
        ],
        ..Scenario::default()
    };
    let mut harness = Harness::new(scenario, WIDE);
    // Sequences 0 to 3, then the one the PID changes hands on.
    harness.run(4);
    harness.select(original);
    let _ = harness.press('p');
    assert!(harness.state.is_pinned(original), "§2.5: `p` pins the row");

    harness.run(1);

    let snapshot = harness.state.snapshot().expect("a frame is on screen");
    assert!(
        snapshot.process(recycled).is_some(),
        "the scenario should have handed the PID to a different process"
    );
    assert!(
        snapshot.process(original).is_none(),
        "and the original identity should no longer resolve"
    );
    assert!(
        recycled.is_reuse_of(&original),
        "the two identities really are the same PID"
    );

    // §26: a PID is not an identity. The recycled process is warming up, and an
    // unavailable value sorts last, so it is *not* at the row the selection
    // remembered — the position fallback lands on a survivor instead.
    let selected = harness.state.selected().expect("something is selected");
    assert_ne!(
        selected, recycled,
        "a recycled PID must not inherit the cursor"
    );
    assert_ne!(
        selected, original,
        "and neither may a dead identity keep it"
    );
    assert_eq!(
        selected, survivor,
        "§7.2: the remembered row now belongs to the surviving process"
    );

    // The pin is keyed by identity, so nothing carried over there either.
    assert!(
        !harness.state.is_pinned(recycled),
        "§2.5, §26: a recycled PID must not inherit a pin"
    );
    assert!(
        harness.state.is_pinned(original),
        "the pin still names the process the user pinned, not whatever holds its PID"
    );
    let frame = harness.draw();
    assert!(
        frame.contains("keeper-low"),
        "the surviving rows are still on screen:\n{frame}"
    );
}

// --------------------------------------------------------------- config reload

/// Performs `Effect::ReloadConfig` the way `interactive.rs` performs it.
///
/// Mirrored rather than called: the binary's handler is a private function inside
/// its event loop. What is *not* mirrored is where the outcome goes — the real
/// handler forwards it as `Event::ConfigReloaded`, and only replaces the running
/// configuration when the reload fully succeeded, which is the §12 atomicity these
/// two tests exercise.
fn perform_reload(running: &mut Config, path: &Path) -> ConfigOutcome {
    match config::reload(running, path) {
        // Adopted even when non-reloadable keys changed, which is what
        // `interactive.rs` does: §12 asks for those keys to be *identified and
        // explained*, and refusing the whole candidate over one of them left the
        // running configuration untouched behind a message that said "reloaded".
        // What the interactive runtime adds on top — the state, the sampler thread,
        // and the notice — is unit-tested where those live, because reaching them
        // from here would mean including the terminal setup as well.
        Ok(outcome) => {
            let reloaded = Box::new(outcome.config);
            *running = *reloaded.clone();
            Ok(reloaded)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Executes the `Effect::ReloadConfig` in `effects`, if the reducer emitted one.
///
/// Going through the effect list rather than calling [`perform_reload`] directly is
/// what makes these tests integration tests: the configuration layer is reachable
/// only because the reducer asked for it (§10.5).
fn execute_reload(effects: &Effects, running: &mut Config, path: &Path) -> Option<ConfigOutcome> {
    effects.iter().find_map(|effect| match effect {
        Effect::ReloadConfig => Some(perform_reload(running, path)),
        _ => None,
    })
}

#[test]
fn a_valid_reload_replaces_the_running_configuration() {
    let scratch = ScratchDir::new("reload-valid");
    let path = scratch.write(
        "monitrs.toml",
        "config_version = 1\n\
         [sampling]\n\
         interval = \"2s\"\n\
         history = \"120s\"\n\
         [display]\n\
         theme = \"high-contrast\"\n",
    );
    let mut running = Config::default();
    assert_eq!(running.sampling.interval, Duration::from_secs(1));

    let mut harness = Harness::with_settings(Scenario::default(), WIDE, |settings| {
        settings.config_path = Some(path.clone());
    });
    harness.run(2);

    // `:reload config` produces an effect and performs nothing itself (§10.5).
    let _ = harness.press(':');
    harness.type_text("reload config");
    let effects = harness.press_key(Key::Enter);

    let outcome = execute_reload(&effects, &mut running, &path)
        .expect("§6.3: the palette command must ask for a reload");

    assert!(outcome.is_ok(), "a valid file must reload: {outcome:?}");
    assert_eq!(
        running.sampling.interval,
        Duration::from_secs(2),
        "§12: a valid reload applies"
    );
    assert_eq!(running.sampling.history, Duration::from_secs(120));
    assert_eq!(running.display.theme, "high-contrast");

    // The reducer takes the payload without inspecting it (§10.1) and repaints.
    let effects = apply(&mut harness.state, Event::ConfigReloaded(outcome));
    assert!(effects.contains(&Effect::RequestRedraw), "{effects:?}");
    let frame = harness.draw();
    assert!(
        frame.contains("rustc"),
        "the interface keeps working across a reload:\n{frame}"
    );
}

#[test]
fn an_invalid_reload_leaves_the_running_configuration_untouched() {
    let scratch = ScratchDir::new("reload-invalid");
    let path = scratch.write(
        "monitrs.toml",
        // Parses, but 10 ms is below the floor §12 allows.
        "config_version = 1\n\
         [sampling]\n\
         interval = \"10ms\"\n",
    );
    let mut running = Config::default();
    let before = running.clone();

    let mut harness = Harness::with_settings(Scenario::default(), WIDE, |settings| {
        settings.config_path = Some(path.clone());
    });
    harness.run(2);
    let displayed = harness.displayed_sequence();

    let _ = harness.press(':');
    harness.type_text("reload config");
    let effects = harness.press_key(Key::Enter);

    let outcome = execute_reload(&effects, &mut running, &path)
        .expect("§6.3: the palette command must ask for a reload");

    let message = outcome.expect_err("an out-of-range value must be refused");
    assert!(
        message.contains("sampling.interval"),
        "§12: the refusal must name the key, got {message:?}"
    );
    assert_eq!(
        running, before,
        "§12: a reload that failed validation must change nothing"
    );

    // The failure reaches the reducer as a payload, and the interface survives it.
    let payload: ConfigOutcome = Err(message);
    let effects = apply(&mut harness.state, Event::ConfigReloaded(payload));
    assert!(effects.contains(&Effect::RequestRedraw), "{effects:?}");
    assert_eq!(
        harness.displayed_sequence(),
        displayed,
        "a refused reload must not disturb what is on screen"
    );
    assert_eq!(
        harness.state.sample_interval(),
        Duration::from_secs(1),
        "the running interval is still the one that was validated at startup"
    );
    let frame = harness.draw();
    assert!(frame.contains("LIVE"), "{frame}");

    // And a file that is not even TOML is refused the same way.
    let broken = scratch.write("broken.toml", "[sampling\ninterval = \"1s\"\n");
    let mut still_running = Config::default();
    let error =
        perform_reload(&mut still_running, &broken).expect_err("a malformed file must be refused");
    assert!(error.contains("not valid configuration"), "got {error:?}");
    assert_eq!(still_running, Config::default());

    // A path that does not exist is an error, not a silent fall back to defaults.
    let missing = scratch.path("absent.toml");
    let mut untouched = Config::default();
    let error =
        perform_reload(&mut untouched, &missing).expect_err("a missing file must be refused");
    assert!(error.contains("cannot read"), "got {error:?}");
    assert_eq!(untouched, Config::default());
}

// -------------------------------------------------------------- worker shutdown

#[test]
fn every_worker_joins_on_shutdown_and_a_second_shutdown_is_harmless() {
    let (sender, receiver) = event_channel::<()>();
    let (detail_tx, detail_rx) = detail_channel();
    let shutdown = Shutdown::new();
    let mut workers = Workers::new();

    // A collector slow enough that shutdown lands mid-collection: §10.3 requires
    // every worker to be joined, including one asleep inside a read.
    let slow = Scenario {
        collect_delay: Duration::from_millis(150),
        ..Scenario::default()
    };
    spawn_sampler_thread(
        &mut workers,
        FakeCollector::new(slow.clone()),
        sender.clone(),
        shutdown.clone(),
        SamplingControl::new(Duration::from_millis(10), Thresholds::default()),
        SampleRequest::new(),
        SensorInterest::new(),
    )
    .expect("the sampler thread must spawn");
    spawn_detail_worker(
        &mut workers,
        FakeCollector::new(slow),
        detail_rx,
        sender.clone(),
        shutdown.clone(),
    )
    .expect("the detail worker must spawn");
    spawn_tick_thread(
        &mut workers,
        sender,
        shutdown.clone(),
        Duration::from_millis(10),
    )
    .expect("the tick thread must spawn");
    assert_eq!(workers.len(), 3, "sampler, detail worker, and tick");

    // Wait until each of the three has actually produced something, so the test
    // is not joining threads that never started.
    detail_tx
        .send(runtime::DetailRequest::Fetch(ProcessIdentity::new(
            31_842, 900_100,
        )))
        .expect("the detail request channel accepts one request");
    let mut saw_snapshot = false;
    let mut saw_detail = false;
    let mut saw_tick = false;
    assert!(
        wait_until(|| {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    TestEvent::Snapshot(_) => saw_snapshot = true,
                    TestEvent::Detail(_) => saw_detail = true,
                    TestEvent::Tick(_) => saw_tick = true,
                    _ => {}
                }
            }
            saw_snapshot && saw_detail && saw_tick
        }),
        "the workers did not all report in: snapshot {saw_snapshot}, detail \
         {saw_detail}, tick {saw_tick}"
    );

    shutdown.trigger();
    shutdown.trigger();
    assert!(
        shutdown.is_triggered(),
        "§10.3: the shutdown token is idempotent"
    );
    // The receiver stays alive across the join. A dropped receiver would tell every
    // worker to stop for a second reason, which would hide a worker that ignores
    // the token; keeping it means the token is the only thing being tested. It is
    // safe to leave the channel undrained because a snapshot and a tick are
    // coalescable — a full channel discards them rather than blocking the sender —
    // and the one worker that can block, the detail worker, waits 100 ms at most.
    let stuck = workers.join_all();
    drop(receiver);

    assert!(
        stuck.is_empty(),
        "§10.3: every worker must join on shutdown; these did not: {stuck:?}"
    );
    shutdown.trigger();
    assert!(
        shutdown.is_triggered(),
        "a shutdown after the workers are gone must be harmless"
    );
}

// ------------------------------------------------------------ export redaction

/// Executes the `Effect::ExportSnapshot` in `effects`, as `interactive.rs` does.
///
/// Both details that make this the *normal* path are taken from the binary rather
/// than chosen here: the target comes from the effect, and the policy is
/// [`RedactionPolicy::REDACTED`] with no argument by which a caller could opt out
/// (§15.2). The live snapshot is the subject for the same reason the real handler
/// uses it — an export is a record of now, not of whatever the view is frozen on.
fn execute_export(effects: &Effects, state: &AppState) -> Option<PathBuf> {
    effects.iter().find_map(|effect| match effect {
        Effect::ExportSnapshot(path) => {
            let snapshot = state
                .live_snapshot()
                .expect("a sample has been collected by now");
            let json = SnapshotExport::new(snapshot, RedactionPolicy::REDACTED)
                .to_json()
                .expect("the export serializes");
            std::fs::write(path, json.as_bytes()).expect("write the export");
            Some(path.clone())
        }
        _ => None,
    })
}

#[test]
fn a_snapshot_exported_through_the_palette_carries_no_process_arguments() {
    let scratch = ScratchDir::new("export");
    let path = scratch.path("snapshot.json");
    let scenario = Scenario {
        processes: vec![
            FakeProcess::new(
                4_242,
                500_100,
                "psql",
                "psql postgres://admin:hunter2@db.internal/prod",
            )
            .with_cpu(Pattern::Steady(3.0)),
            FakeProcess::new(31_842, 900_100, "rustc", "cargo build --release")
                .with_cpu(Pattern::Steady(120.0)),
        ],
        ..Scenario::default()
    };
    let mut harness = Harness::new(scenario, WIDE);
    harness.run(4);

    // `:export snapshot <path>` — a reducer effect, never a write (§10.5).
    let _ = harness.press(':');
    harness.type_text(&format!("export snapshot {}", path.display()));
    let effects = harness.press_key(Key::Enter);

    assert!(
        effects.contains(&Effect::ExportSnapshot(path.clone())),
        "§6.3: the palette must ask for an export, got {effects:?}"
    );
    assert!(
        !path.exists(),
        "§10.5: the reducer must not have written anything itself"
    );

    let target =
        execute_export(&effects, &harness.state).expect("§6.3: the palette must ask for an export");
    assert_eq!(target, path, "the export went where the effect said");

    let written = std::fs::read_to_string(&path).expect("read the export back");
    assert!(
        !written.contains("hunter2"),
        "§15.2: a credential in a command line reached the export"
    );
    assert!(
        !written.contains("postgres://"),
        "§15.2: a connection string reached the export"
    );
    assert!(
        !written.contains("--release"),
        "§15.2: arguments are redacted by default, not just secret-looking ones"
    );
    assert!(
        written.contains("psql"),
        "the program itself must stay identifiable"
    );
    assert!(
        written.contains("\"arguments_redacted\": true"),
        "the export must record what it withheld"
    );
    assert!(
        written.contains("\"environment_excluded\": true"),
        "§15.2: environment values are never read, and the export says so"
    );

    let parsed: serde_json::Value = serde_json::from_str(&written).expect("valid JSON");
    let processes = parsed
        .pointer("/processes")
        .and_then(serde_json::Value::as_array)
        .expect("the export lists processes");
    assert_eq!(processes.len(), 2);
    for process in processes {
        let command = process
            .pointer("/command")
            .and_then(serde_json::Value::as_str)
            .expect("every process exports a command");
        assert!(
            !command.contains(' '),
            "§15.2: {command:?} still carries its arguments"
        );
    }
}
