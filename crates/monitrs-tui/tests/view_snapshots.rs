//! Snapshot tests for the five screens and their chrome (§17.3).
//!
//! Every fixture comes from `monitrs-collectors`' `FakeCollector`, driven through
//! the real reducer: a snapshot is delivered as an [`Event::Snapshot`] exactly as
//! the runtime would deliver it, so the history ring fills, the selection
//! initialises, and the frame under test is the frame a user would see. §17.3 asks
//! for states a real machine cannot be put into on demand — the warming-up first
//! frame, permission-denied metrics, stale data, an empty process list — and §17.5
//! requires them to come from a deterministic fake rather than from hand-written
//! structs.
//!
//! The Pressure Radar is derived the way §11's ownership boundary says it must be:
//! the collector emits `PressureSnapshot::warming_up`, the runtime records the
//! sample into the ring and then asks the [`PressureEngine`] for the states. The
//! harness below does the same, so the radar rows in these snapshots are the rows
//! the diagnostic engine really produces rather than a plausible-looking fixture.
//!
//! Two things are snapshotted, not one:
//!
//! * the **characters**, through ratatui's `TestBackend`, which is what a user
//!   reads; and
//! * the **styles**, as a run-length map, because a `TestBackend` view discards
//!   them entirely — and a no-colour snapshot identical to a true-colour one would
//!   prove nothing about §5.2's "colour is never the only indicator".
//!
//! Nondeterminism is excluded rather than normalised: the fake host name is fixed,
//! uptime is a function of the sample sequence, wall time advances one second per
//! sample from the Unix epoch, and no screen reads a clock of its own (§10.5).
//!
//! [`Event::Snapshot`]: monitrs_tui::event::Event::Snapshot
//! [`PressureEngine`]: monitrs_core::diagnostics::PressureEngine

// An integration test is its own crate, so the library's `cfg(test)` allowance
// does not reach here. `expect` is how a test asserts a precondition: a fixture
// that cannot be built is a broken test, and failing loudly at that line is the
// wanted behaviour. Production code keeps both lints denied (§18.2).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use core::fmt::Write as _;
use core::time::Duration;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use monitrs_collectors::fake::{FakeCollector, FakeProcess, Pattern, Scenario};
use monitrs_collectors::source::{SampleTick, SnapshotSource};
use monitrs_collectors::tier::DueTiers;
use monitrs_core::diagnostics::{PressureEngine, Thresholds};
use monitrs_core::model::{CollectorHealth, ProcessState, SelfOverhead, TierHealth};
use monitrs_core::process::{ProcessSort, ProcessSortKey};
use monitrs_core::units::Percent;
use monitrs_tui::action::ViewId;
use monitrs_tui::app::{AppSettings, AppState};
use monitrs_tui::event::{Event, Key, KeyPress, TerminalEvent};
use monitrs_tui::glyphs::GlyphSet;
use monitrs_tui::theme::{ColorDepth, ThemeId};
use monitrs_tui::views;
use monitrs_tui::widgets::Presentation;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// How the state under test should be configured before the snapshots arrive.
#[derive(Clone, Debug)]
struct Fixture {
    scenario: Scenario,
    samples: u64,
    size: (u16, u16),
    view: ViewId,
    tree: bool,
    sort: ProcessSort,
    /// Extra collector health, for the Inspect screen's diagnostics section.
    health: Option<CollectorHealth>,
    /// Whether to freeze the timeline after the last sample (§2.1).
    paused: bool,
    /// A PID whose subtree the table is scoped to (§6.2 `F`).
    following: Option<u32>,
}

impl Fixture {
    /// The reference scenario at `size`, on `view`.
    fn new(size: (u16, u16), view: ViewId) -> Self {
        Self {
            scenario: Scenario::default(),
            samples: 24,
            size,
            view,
            tree: false,
            sort: ProcessSort::descending(ProcessSortKey::Cpu),
            health: None,
            paused: false,
            following: None,
        }
    }

    fn with_scenario(mut self, scenario: Scenario) -> Self {
        self.scenario = scenario;
        self
    }

    fn with_samples(mut self, samples: u64) -> Self {
        self.samples = samples;
        self
    }

    fn with_tree(mut self, tree: bool) -> Self {
        self.tree = tree;
        self
    }

    fn with_sort(mut self, sort: ProcessSort) -> Self {
        self.sort = sort;
        self
    }

    fn with_health(mut self, health: CollectorHealth) -> Self {
        self.health = Some(health);
        self
    }

    fn paused(mut self) -> Self {
        self.paused = true;
        self
    }

    /// Follows `pid`'s subtree once the samples have arrived (§6.2 `F`).
    ///
    /// Recorded as a PID rather than an identity because that is what the palette takes,
    /// and the start key is the snapshot's to supply.
    fn following(mut self, pid: u32) -> Self {
        self.following = Some(pid);
        self
    }

    /// Builds the state by driving the reducer with the fake collector's output.
    fn build(self) -> AppState {
        let mut state = AppState::new(AppSettings {
            started_at: Instant::now(),
            size: self.size,
            view: self.view,
            tree_mode: self.tree,
            sort: self.sort,
            ..AppSettings::default()
        });
        let mut collector = FakeCollector::new(self.scenario);
        let mut engine = PressureEngine::new(Thresholds::default());
        let start = Instant::now();
        let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);

        for index in 0..self.samples {
            if index > 0 {
                tick = tick.advance(
                    start + Duration::from_secs(index),
                    SystemTime::UNIX_EPOCH + Duration::from_secs(index),
                    DueTiers::ALL,
                );
            }
            let Ok(mut snapshot) = collector.sample(&tick) else {
                continue;
            };
            // §11's ownership boundary, in the order it documents: record the sample,
            // then derive the radar from it.
            snapshot.pressure = engine.observe(&snapshot);
            let _ = monitrs_tui::app::apply(&mut state, Event::<()>::Snapshot(Arc::new(snapshot)));
        }
        if let Some(health) = self.health {
            let _ = monitrs_tui::app::apply(&mut state, Event::<()>::health(health));
        }
        if let Some(pid) = self.following {
            // Typed into the palette rather than set on the state, because that is the
            // only way a user can reach it — and `followed` is private to the app module
            // for exactly that reason.
            type_command(&mut state, &format!("follow {pid}"));
        }
        if self.paused {
            let _ = monitrs_tui::app::reduce(&mut state, monitrs_tui::action::Action::TogglePause);
        }
        // One recorded frame, so the Inspect screen's render timing is not empty. The
        // duration is fixed rather than measured: §17.3 forbids nondeterminism.
        let at = state.clock() + Duration::from_millis(100);
        state.record_render(at, Duration::from_millis(4));
        state
    }
}

/// Types a line into the command palette and submits it (§6.3).
fn type_command(state: &mut AppState, line: &str) {
    let mut press = |key: KeyPress| {
        let _ = monitrs_tui::app::apply::<()>(state, Event::Terminal(TerminalEvent::Key(key)));
    };
    press(KeyPress::char(':'));
    for character in line.chars() {
        press(KeyPress::char(character));
    }
    press(KeyPress::plain(Key::Enter));
}

/// A scenario whose host name is far longer than any header (§17.3).
fn long_hostname_scenario() -> Scenario {
    Scenario {
        hostname: "build-agent-eu-west-1b-really-quite-long-hostname.example.internal".into(),
        ..Scenario::default()
    }
}

/// A scenario with the zombie and uninterruptible-sleep rows §7.2 singles out.
fn notable_states_scenario() -> Scenario {
    Scenario {
        processes: vec![
            FakeProcess::new(31_842, 900_100, "rustc", "cargo build --release")
                .with_cpu(Pattern::Steady(287.0))
                .with_rss(2_814_509_056),
            FakeProcess::new(1_221, 700_050, "postgres", "postgres: checkpointer")
                .with_cpu(Pattern::Steady(0.0))
                .with_state(ProcessState::Zombie)
                .with_user("postgres", 70),
            FakeProcess::new(4_410, 660_000, "nfsd", "nfsd")
                .with_cpu(Pattern::Steady(0.0))
                .with_state(ProcessState::UninterruptibleSleep)
                .with_user("root", 0),
            FakeProcess::new(1, 1, "launchd", "/sbin/launchd").with_cpu(Pattern::Steady(0.1)),
        ],
        ..Scenario::default()
    }
}

/// A process tree with real depth, for the tree-mode snapshot.
fn tree_scenario() -> Scenario {
    let mut processes = vec![
        FakeProcess::new(1, 1, "launchd", "/sbin/launchd").with_cpu(Pattern::Steady(0.1)),
        FakeProcess::new(500, 2, "sshd", "sshd: gabor").with_cpu(Pattern::Steady(1.0)),
        FakeProcess::new(501, 3, "bash", "-bash").with_cpu(Pattern::Steady(2.0)),
        FakeProcess::new(502, 4, "cargo", "cargo build --release").with_cpu(Pattern::Steady(9.0)),
        FakeProcess::new(503, 5, "rustc", "rustc --crate-name monitrs")
            .with_cpu(Pattern::Steady(287.0)),
        FakeProcess::new(504, 6, "rustc", "rustc --crate-name monitrs_core")
            .with_cpu(Pattern::Steady(140.0)),
        FakeProcess::new(600, 7, "cron", "/usr/sbin/cron").with_cpu(Pattern::Steady(0.0)),
    ];
    // launchd -> {sshd -> bash -> cargo -> {rustc, rustc}, cron}
    for (pid, parent) in [
        (500u32, 1u32),
        (501, 500),
        (502, 501),
        (503, 502),
        (504, 502),
        (600, 1),
    ] {
        if let Some(process) = processes
            .iter_mut()
            .find(|process| process.identity.pid == pid)
        {
            process.parent_pid = Some(parent);
        }
    }
    Scenario {
        processes,
        ..Scenario::default()
    }
}

/// Collector health with something to report in every §7.5 diagnostics field.
fn busy_health() -> CollectorHealth {
    let mut health = CollectorHealth {
        fast: TierHealth {
            last_duration: Duration::from_millis(3),
            max_duration: Duration::from_millis(11),
            p95_duration: Duration::from_millis(5),
            completed: 24,
            failed: 1,
            since_last: Some(Duration::from_millis(200)),
        },
        medium: TierHealth {
            last_duration: Duration::from_millis(14),
            max_duration: Duration::from_millis(31),
            p95_duration: Duration::from_millis(22),
            completed: 5,
            failed: 0,
            since_last: Some(Duration::from_secs(2)),
        },
        dropped_samples: 2,
        coalesced_samples: 7,
        lag: Duration::from_millis(1_400),
        self_overhead: Some(SelfOverhead {
            cpu: Percent::new(0.6).expect("finite"),
            rss_bytes: 23 * 1024 * 1024,
            history_bytes: 2 * 1024 * 1024 + 512 * 1024,
            open_files: monitrs_core::model::MetricState::Available(14),
        }),
        ..CollectorHealth::default()
    };
    health.record_issue(
        "/proc/diskstats",
        "permission denied",
        Duration::from_secs(3),
    );
    health.record_issue(
        "/proc/diskstats",
        "permission denied",
        Duration::from_secs(2),
    );
    health.record_issue("hwmon", "no sensors found", Duration::from_secs(30));
    health
}

// ---------------------------------------------------------------------------
// Rendering harness
// ---------------------------------------------------------------------------

/// The three presentations §17.3 names: strict ASCII, enhanced Unicode, no colour.
fn ascii() -> Presentation<'static> {
    Presentation::new(
        GlyphSet::ascii(),
        ThemeId::DefaultDark.theme(),
        ColorDepth::TrueColor,
    )
}

fn unicode() -> Presentation<'static> {
    ascii().with_glyphs(GlyphSet::unicode())
}

fn no_color() -> Presentation<'static> {
    ascii().with_depth(ColorDepth::Off)
}

/// Renders one whole frame of `state` and returns the buffer.
fn frame(state: &AppState, presentation: Presentation<'_>) -> Buffer {
    let (width, height) = state.size();
    let mut terminal = Terminal::new(TestBackend::new(width.max(1), height.max(1)))
        .expect("a test backend never fails to initialise");
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height);
            views::render(frame, area, state, presentation);
        })
        .expect("drawing to a test backend never fails");
    terminal.backend().buffer().clone()
}

/// The characters of a buffer, one framed line per row.
///
/// The frame makes trailing whitespace visible, which matters: a panel that stops
/// one cell short of its border is a §5.4 bug and an unframed snapshot hides it.
///
/// A double-width grapheme occupies two cells and ratatui blanks the second, so
/// the continuation cell is skipped rather than printed as a space.
fn text_of(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in buffer.area.top()..buffer.area.bottom() {
        out.push('|');
        let mut skip = 0u16;
        for x in buffer.area.left()..buffer.area.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            match buffer.cell((x, y)) {
                Some(cell) => {
                    let symbol = cell.symbol();
                    out.push_str(symbol);
                    skip = u16::try_from(monitrs_core::units::display_width(symbol))
                        .unwrap_or(1)
                        .saturating_sub(1);
                }
                None => out.push('?'),
            }
        }
        out.push('|');
        out.push('\n');
    }
    out
}

/// A run-length map of the styles in a buffer.
fn styles_of(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in buffer.area.top()..buffer.area.bottom() {
        let _ = write!(out, "{y:>2}:");
        let mut run: Option<(String, u16)> = None;
        for x in buffer.area.left()..buffer.area.right() {
            let Some(cell) = buffer.cell((x, y)) else {
                continue;
            };
            let key = format!("{:?}/{:?}/{:?}", cell.fg, cell.bg, cell.modifier);
            match &mut run {
                Some((current, count)) if *current == key => *count += 1,
                Some((current, count)) => {
                    let _ = write!(out, " {count}x{current}");
                    run = Some((key, 1));
                }
                None => run = Some((key, 1)),
            }
        }
        if let Some((current, count)) = run {
            let _ = write!(out, " {count}x{current}");
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// §17.3: breakpoints
// ---------------------------------------------------------------------------

#[test]
fn wide_live_overview() {
    let state = Fixture::new((140, 38), ViewId::Overview).build();
    let text = text_of(&frame(&state, ascii()));
    // §2.1: the header says which of the three timeline states this is.
    assert!(text.contains("[>LIVE]"), "{text}");
    // §5.5: the process panel's trailing count.
    assert!(text.contains(" total "), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn wide_live_overview_in_unicode_mode() {
    let state = Fixture::new((140, 38), ViewId::Overview).build();
    insta::assert_snapshot!(text_of(&frame(&state, unicode())));
}

#[test]
fn standard_layout() {
    // §5.7: header meters, compact history, process table, one focus-selected
    // lower summary panel.
    let state = Fixture::new((110, 30), ViewId::Overview).build();
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

#[test]
fn compact_eighty_by_twenty_four() {
    // §5.7's Compact band, and the size every terminal is guaranteed to have.
    let state = Fixture::new((80, 24), ViewId::Overview).build();
    let text = text_of(&frame(&state, ascii()));
    for line in text.lines() {
        assert_eq!(
            monitrs_core::units::display_width(line),
            82,
            "a row is not exactly 80 cells wide: {line:?}"
        );
    }
    insta::assert_snapshot!(text);
}

#[test]
fn too_small_layout_keeps_a_minimal_process_list() {
    // §5.7: a stable minimal process list down to 60x16.
    let state = Fixture::new((60, 16), ViewId::Overview).build();
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

#[test]
fn too_small_layout_shows_the_resize_notice() {
    // Below 60x16 the interface is replaced by §5.7's three lines, verbatim.
    let state = Fixture::new((52, 12), ViewId::Overview).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("monitrs needs at least 60x16"), "{text}");
    assert!(text.contains("current terminal: 52x12"), "{text}");
    insta::assert_snapshot!(text);
}

// ---------------------------------------------------------------------------
// §17.3: glyph and colour modes
// ---------------------------------------------------------------------------

#[test]
fn ascii_mode_emits_only_printable_seven_bit_output() {
    // §5.1's crate-wide promise, asserted over a whole frame rather than a widget.
    let state = Fixture::new((140, 38), ViewId::Processes).build();
    let buffer = frame(&state, ascii());
    for cell in buffer.content() {
        for byte in cell.symbol().bytes() {
            assert!(
                (0x20..=0x7e).contains(&byte),
                "strict ASCII mode emitted {:?}",
                cell.symbol()
            );
        }
    }
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn unicode_mode_uses_the_enhanced_character_set() {
    let state = Fixture::new((140, 38), ViewId::Processes).build();
    insta::assert_snapshot!(text_of(&frame(&state, unicode())));
}

#[test]
fn no_color_mode_renders_the_same_characters() {
    let state = Fixture::new((140, 38), ViewId::Overview).build();
    let coloured = frame(&state, ascii());
    let plain = frame(&state, no_color());
    assert_eq!(
        text_of(&coloured),
        text_of(&plain),
        "§5.2: turning colour off must not lose a character"
    );
    assert_ne!(
        styles_of(&coloured),
        styles_of(&plain),
        "the two depths must be distinguishable at all"
    );
    insta::assert_snapshot!(text_of(&plain));
}

#[test]
fn no_color_mode_styles() {
    // The characters are identical with colour on and off, so this is the snapshot
    // that shows meaning survives: §5.2's states still differ by modifier.
    let state = Fixture::new((100, 28), ViewId::Overview).build();
    insta::assert_snapshot!(styles_of(&frame(&state, no_color())));
}

#[test]
fn true_color_mode_styles() {
    let state = Fixture::new((100, 28), ViewId::Overview).build();
    insta::assert_snapshot!(styles_of(&frame(&state, ascii())));
}

// ---------------------------------------------------------------------------
// §17.3: metric states
// ---------------------------------------------------------------------------

#[test]
fn empty_process_list() {
    // A locked-down container really can show nothing, and it is not a failure.
    let state = Fixture::new((140, 38), ViewId::Processes)
        .with_scenario(Scenario::empty())
        .build();
    assert_eq!(state.rows().len(), 0);
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("no processes visible"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn permission_denied_metrics() {
    let state = Fixture::new((140, 38), ViewId::Overview)
        .with_scenario(Scenario::permission_denied())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(
        text.contains("permission denied") || text.contains("n/a"),
        "{text}"
    );
    // §4: never silently zero.
    assert!(
        !text.contains(" 0% ["),
        "a denied meter drew a zero bar:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn permission_denied_metrics_on_the_inspect_screen() {
    // §7.5 asks for the unavailable metrics *and why*, which is the panel that
    // turns a wall of `n/a` into an explanation.
    let state = Fixture::new((140, 38), ViewId::Inspect)
        .with_scenario(Scenario::permission_denied())
        .with_health(busy_health())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("UNAVAILABLE METRICS"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn stale_data() {
    // §4: a retained value may be shown only if it is visibly marked stale *and*
    // carries its age.
    let state = Fixture::new((140, 38), ViewId::Overview)
        .with_scenario(Scenario {
            stale_from: Some(6),
            ..Scenario::default()
        })
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains('~'), "the stale marker is missing:\n{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn stale_data_is_explained_on_the_inspect_screen() {
    let state = Fixture::new((140, 38), ViewId::Inspect)
        .with_scenario(Scenario {
            stale_from: Some(6),
            ..Scenario::default()
        })
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("STALE DATA"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn warming_up_first_frame() {
    // §8.2, §26: the first delta sample is warming up, never zero. This snapshot is
    // the one that would change if a screen ever rendered `0%` here.
    let state = Fixture::new((140, 38), ViewId::Overview)
        .with_samples(1)
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(
        text.contains("warming up") || text.contains("n/a"),
        "{text}"
    );
    assert!(
        !text.contains("0B/s"),
        "a warming-up rate read as zero:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn the_very_first_frame_before_any_snapshot() {
    // The runtime draws before the collector has answered at all.
    let state = Fixture::new((140, 38), ViewId::Overview)
        .with_samples(0)
        .build();
    assert!(state.snapshot().is_none());
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

// ---------------------------------------------------------------------------
// §17.3: names, cores, and modes
// ---------------------------------------------------------------------------

#[test]
fn a_long_hostname() {
    // The header must stay inside its frame, and the title is what gives way.
    let state = Fixture::new((100, 28), ViewId::Overview)
        .with_scenario(long_hostname_scenario())
        .build();
    let text = text_of(&frame(&state, ascii()));
    for line in text.lines() {
        assert_eq!(
            monitrs_core::units::display_width(line),
            102,
            "a row is not exactly 100 cells wide: {line:?}"
        );
    }
    insta::assert_snapshot!(text);
}

#[test]
fn a_long_hostname_in_a_compact_header() {
    let state = Fixture::new((80, 24), ViewId::Overview)
        .with_scenario(long_hostname_scenario())
        .build();
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

#[test]
fn a_high_core_count() {
    // §7.1: hundreds of cores are aggregated into a strip, never rendered as rows.
    let state = Fixture::new((140, 38), ViewId::Overview)
        .with_scenario(Scenario::many_cores())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert_eq!(
        text.lines().count(),
        38,
        "256 cores must not add rows to the frame"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn tree_mode() {
    let state = Fixture::new((140, 38), ViewId::Processes)
        .with_scenario(tree_scenario())
        .with_tree(true)
        .with_sort(ProcessSort::ascending(ProcessSortKey::Pid))
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("tree"), "the panel says which mode it is in");
    assert!(text.contains("`-") || text.contains("+-"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn tree_mode_in_unicode_mode() {
    let state = Fixture::new((140, 38), ViewId::Processes)
        .with_scenario(tree_scenario())
        .with_tree(true)
        .with_sort(ProcessSort::ascending(ProcessSortKey::Pid))
        .build();
    insta::assert_snapshot!(text_of(&frame(&state, unicode())));
}

#[test]
fn notable_process_states_are_visibly_distinct_without_colour() {
    // §7.2: zombie and uninterruptible-sleep rows. The marker column carries the
    // state code so the cue survives the STATE column being dropped.
    let state = Fixture::new((100, 28), ViewId::Processes)
        .with_scenario(notable_states_scenario())
        .with_sort(ProcessSort::ascending(ProcessSortKey::Pid))
        .build();
    let text = text_of(&frame(&state, no_color()));
    assert!(text.contains("|Z"), "a zombie row lost its marker:\n{text}");
    assert!(
        text.contains("|D"),
        "a D-state row lost its marker:\n{text}"
    );
    insta::assert_snapshot!(text);
}

// ---------------------------------------------------------------------------
// §17.3: the remaining screens
// ---------------------------------------------------------------------------

#[test]
fn the_processes_screen() {
    let state = Fixture::new((140, 38), ViewId::Processes).build();
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

#[test]
fn the_cpu_screen() {
    // The heterogeneous case, which is the one the screen exists for: the fake platform
    // reports two core classes, so the cores arrive grouped and named rather than as a
    // flat list of numbers that mean different things depending on which core they are.
    let state = Fixture::new((140, 38), ViewId::Cpu).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("PERFORMANCE"), "{text}");
    assert!(text.contains("EFFICIENCY"), "{text}");
    // The load average is useless without knowing how many cores it is spread over.
    assert!(text.contains("per core"), "{text}");
    // And the processes accounting for it, because a busy core raises exactly that
    // question.
    assert!(text.contains("BUSIEST PROCESSES"), "{text}");
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

#[test]
fn the_cpu_screen_on_a_machine_with_one_kind_of_core() {
    // A homogeneous machine reports no classes, and one group is no classification: the
    // panel is drawn unlabelled rather than inventing a name for the only kind there is.
    let scenario = Scenario {
        asymmetric_cores: false,
        ..Scenario::default()
    };
    let state = Fixture::new((140, 38), ViewId::Cpu)
        .with_scenario(scenario)
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(
        text.contains("CORES"),
        "the one group is unlabelled:\n{text}"
    );
    assert!(
        !text.contains("PERFORMANCE") && !text.contains("EFFICIENCY"),
        "a machine that reports no classes must not be given invented ones:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn the_cpu_screen_while_warming_up() {
    // §8.2: the first frame has no rates at all, so every core and the total say so
    // rather than showing zero.
    let state = Fixture::new((140, 38), ViewId::Cpu).with_samples(1).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("warming"), "{text}");
    assert!(
        !text.contains("0%"),
        "a warming-up core must not render as 0%:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn the_storage_screen() {
    // §7.3: four clearly labelled sections, and no unlabelled percentage.
    let state = Fixture::new((140, 38), ViewId::Storage).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("FILESYSTEM CAPACITY"), "{text}");
    assert!(text.contains("DEVICE THROUGHPUT"), "{text}");
    assert!(text.contains("TOP DISK I/O"), "{text}");
    assert!(text.contains("THROUGHPUT HISTORY"), "{text}");
    // The fake platform cannot produce a device busy figure, so the column is not
    // drawn and the panel says why.
    assert!(!text.contains("BUSY"), "{text}");
    // Inodes are a second percentage of a second thing, under their own heading.
    assert!(text.contains("INODE%"), "{text}");
    // Two mounts share the fake APFS container, so both are marked and the panel
    // says what the mark means — without it a reader adds 494G to 494G.
    assert!(text.contains("shares a device"), "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("|=") || line.starts_with("||="))
            .count(),
        2,
        "both shared mounts must carry the marker:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn the_storage_screen_ranks_the_processes_using_the_disk() {
    // The panel the screen exists for: *what is writing to my disk right now*. A real
    // machine has hundreds of processes, so this is the fixture that shows the ranking
    // filling the panel rather than the five-process reference scenario.
    let state = Fixture::new((140, 38), ViewId::Storage)
        .with_scenario(Scenario::with_process_count(40))
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("TOP DISK I/O"), "{text}");
    // Ordered by read+write, so the busiest row is above the quietest.
    let first = text
        .lines()
        .skip_while(|line| !line.contains("TOTAL R"))
        .nth(1)
        .expect("a first ranked row");
    assert!(first.contains("rustc"), "{text}");
    // §2.3, §2.5: a panel that dropped rows says how many.
    assert!(text.contains("21 of 40"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_storage_screen_with_per_process_io_refused() {
    // §4, §9.3: the rows stay — a process whose counters were refused is still a
    // process — and the panel says once why every figure below reads the same.
    let state = Fixture::new((140, 38), ViewId::Storage)
        .with_scenario(Scenario::permission_denied())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("per-process io"), "{text}");
    assert!(
        text.contains("permission denied") || text.contains("denied"),
        "{text}"
    );
    assert!(
        !text.contains("0B/s"),
        "a refused counter must not read as an idle process:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn the_storage_screen_with_nothing_running() {
    // A locked-down container really can show no processes at all, and the I/O panel
    // has to say so rather than looking like a panel that failed to draw.
    let state = Fixture::new((140, 38), ViewId::Storage)
        .with_scenario(Scenario::empty())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("no processes visible"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_storage_screen_while_warming_up() {
    // §8.2: the first sample has no rates at all, so every throughput cell and the
    // history say so rather than showing zero.
    let state = Fixture::new((140, 38), ViewId::Storage)
        .with_samples(1)
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("warming"), "{text}");
    assert!(
        !text.contains("0B/s"),
        "a warming-up rate read as zero:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn the_storage_screen_in_the_compact_band() {
    // §4 permits hiding an optional field where space is scarce, but not hiding the
    // fact that it was hidden: at 80 columns the inode columns go and the panel says
    // they need a wider terminal.
    let state = Fixture::new((80, 24), ViewId::Storage).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(!text.contains("INODE%"), "{text}");
    for line in text.lines() {
        assert_eq!(
            monitrs_core::units::display_width(line),
            82,
            "a row is not exactly 80 cells wide: {line:?}"
        );
    }
    insta::assert_snapshot!(text);
}

#[test]
fn the_network_screen() {
    // §7.4: no utilization percentage without a link speed.
    let state = Fixture::new((140, 38), ViewId::Network).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("INTERFACES"), "{text}");
    assert!(text.contains("launch rx"), "{text}");
    assert!(text.contains("os rx"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_network_screen_with_a_known_link_speed() {
    // The other half of §7.4's rule: where the speed is known, the figure is real.
    let state = Fixture::new((140, 38), ViewId::Network)
        .with_scenario(Scenario {
            link_speed_mbps: Some(1_000),
            ..Scenario::default()
        })
        .build();
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

#[test]
fn the_battery_screen() {
    // The laptop case: every field the screen can show, filled in. The three
    // capacity-related figures — 82% charge, 92% health, 48.2 of 52.6 Wh — are
    // deliberately distinct numbers, because a screen where two of them were swapped
    // must be visibly wrong in the snapshot.
    let state = Fixture::new((140, 38), ViewId::Battery).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("BATTERY"), "{text}");
    assert!(text.contains("discharging"), "{text}");
    assert!(text.contains("TO EMPTY"), "{text}");
    assert!(text.contains("52.6 Wh design"), "{text}");
    // §17.3: the thermal sensors get a panel of their own rather than surviving only
    // as the single hottest figure in the Overview header.
    assert!(text.contains("THERMAL SENSORS"), "{text}");
    assert!(text.contains("performance"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_battery_screen_on_a_machine_with_no_battery() {
    // §4 and §26, and the case every CI runner, server, container and desktop hits.
    // The screen must name the absence and its reason, and must not put a zero, an
    // empty bar, or a blank panel where the charge level would go.
    let state = Fixture::new((140, 38), ViewId::Battery)
        .with_scenario(Scenario::no_battery())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("no battery on this machine"), "{text}");
    assert!(text.contains("n/a"), "{text}");
    assert!(
        !text.contains(" 0% "),
        "an absent battery must never render as a charge level:\n{text}"
    );
    assert!(
        !text.contains("TO EMPTY") && !text.contains("CYCLES"),
        "the secondary fields belong to a pack that exists:\n{text}"
    );
    // The thermal sensors are a separate metric and are unaffected by the absence.
    assert!(text.contains("THERMAL SENSORS"), "{text}");
    assert!(text.contains("62.5C"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_battery_screen_while_charging() {
    // Charging inverts the estimate's meaning, and an unlabelled `49m` would be read
    // as time-to-empty by anyone who glanced at the discharging screen first.
    let state = Fixture::new((140, 38), ViewId::Battery)
        .with_scenario(Scenario::charging())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("charging"), "{text}");
    assert!(text.contains("TO FULL"), "{text}");
    assert!(!text.contains("TO EMPTY"), "{text}");
    // §5.2: the charge state carries its own character as well as a colour.
    assert!(text.contains("+charging"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_inspect_screen() {
    let state = Fixture::new((140, 38), ViewId::Inspect)
        .with_health(busy_health())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("SYSTEM"), "{text}");
    assert!(text.contains("SELECTED PROCESS"), "{text}");
    assert!(text.contains("DIAGNOSTICS"), "{text}");
    // §7.5, §15.2: environment-variable values must not appear, and the model has
    // no field for them.
    assert!(!text.contains("PATH="), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_inspect_screen_in_one_column() {
    let state = Fixture::new((90, 26), ViewId::Inspect)
        .with_health(busy_health())
        .build();
    insta::assert_snapshot!(text_of(&frame(&state, ascii())));
}

#[test]
fn the_inspect_screen_in_a_container() {
    // §9.2: a cgroup limit is shown beside the host total, both labelled.
    let state = Fixture::new((140, 38), ViewId::Inspect)
        .with_scenario(Scenario::containerised())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("cgroup limit"), "{text}");
    assert!(text.contains("heuristic"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn following_a_build_scopes_the_table_to_it() {
    // §7.2's table, scoped to one family: `make` with two compilers and an assembler,
    // out of a machine that also has a browser, a database and a window server.
    let state = Fixture::new((140, 38), ViewId::Processes)
        .with_scenario(Scenario::a_build())
        .following(410)
        .build();
    let text = text_of(&frame(&state, ascii()));

    assert!(text.contains("following 410"), "{text}");
    // The `cc` whose CPU the OS refuses is still a member, so the summed CPU is a lower
    // bound and is marked as one. A bare figure here would present three of four
    // compilers as the whole family's cost (§4).
    assert!(
        text.contains("cpu >="),
        "a partial sum must be marked as a lower bound:\n{text}"
    );
    assert!(
        !text.contains("WindowServer"),
        "the machine is out of scope"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn following_a_build_in_tree_mode_reroots_the_tree_on_the_followed_process() {
    // The scope goes through the ordinary filter, so `ProcessTree` re-attaches the
    // children of a hidden process to their nearest surviving ancestor — which for a
    // subtree means the followed root becomes the root of what is drawn, rather than the
    // family's rows being scattered at depth zero.
    let state = Fixture::new((140, 38), ViewId::Processes)
        .with_scenario(Scenario::a_build())
        .with_tree(true)
        .following(410)
        .build();
    let text = text_of(&frame(&state, ascii()));

    assert!(text.contains("following 410"), "{text}");
    assert!(
        !text.contains("zsh"),
        "the parent shell is not in the subtree"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn the_inspect_screen_names_the_container_it_found() {
    // §7.5 answers *whether* with the classification; the identity answers *which*, and
    // it goes ahead of the evidence so a narrow panel truncates "how we guessed" rather
    // than "what this is".
    let state = Fixture::new((140, 38), ViewId::Inspect)
        .with_scenario(Scenario::containerised())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("container docker 3f4a1b2c9d8e"), "{text}");
    // The group's own charge sits with its own limit. The host's `used` appearing here
    // instead would report 23G of 2.0G.
    assert!(text.contains("2.0G, 512M used (25%)"), "{text}");
    assert!(
        !text.contains("23G of 2.0G"),
        "a host figure must never be divided by a container limit:\n{text}"
    );
}

#[test]
fn the_cpu_screen_states_the_ceiling_a_container_actually_has() {
    // Twelve cores drawn for a process that may occupy one and a half of them is the
    // §9.2 failure in its most misleading form: every panel below is about CPUs.
    let state = Fixture::new((140, 38), ViewId::Cpu)
        .with_scenario(Scenario::containerised())
        .build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("cgroup 1.5 CPUs"), "{text}");
    // The load average is the *host's* — `/proc/loadavg` is not namespaced — so the
    // per-core figure is labelled as the machine's rather than the group's, and the
    // quota deliberately does not become the divisor.
    assert!(text.contains("host cores"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn an_unlimited_machine_says_nothing_about_cgroups_on_the_cpu_screen() {
    // The counterpart: the words earned by a container must not appear on a laptop.
    let state = Fixture::new((140, 38), ViewId::Cpu).build();
    let text = text_of(&frame(&state, ascii()));
    assert!(!text.contains("cgroup"), "{text}");
    assert!(!text.contains("host cores"), "{text}");
}

// ---------------------------------------------------------------------------
// §17.3: the Time Lens (§2.1, §26)
// ---------------------------------------------------------------------------

#[test]
fn a_paused_overview_is_unmistakable_from_a_live_one() {
    let state = Fixture::new((140, 38), ViewId::Overview).paused().build();
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("[=PAUSED]"), "{text}");
    assert!(
        text.contains("L live"),
        "§2.1's one explicit action:\n{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn a_history_overview_carries_its_offset_and_a_caret() {
    let mut state = Fixture::new((140, 38), ViewId::Overview).build();
    for _ in 0..8 {
        let _ = monitrs_tui::app::reduce(
            &mut state,
            monitrs_tui::action::Action::SeekHistory(monitrs_tui::action::Seek::Backward(1)),
        );
    }
    let text = text_of(&frame(&state, ascii()));
    assert!(text.contains("[<HISTORY -"), "{text}");
    // §2.5: the caret's note is the selected sample's comparison against its
    // baselines, not merely a repeat of the offset the header already carries.
    assert!(
        text.contains("cpu prev"),
        "the caret's comparison is missing:\n{text}"
    );
    assert!(text.contains('^'), "the caret is missing:\n{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn the_network_caret_note_fits_the_eighty_column_panel() {
    // §2.1's Overview drops its History panel entirely below 100 columns (the
    // `Compact` band), and Storage's THROUGHPUT HISTORY panel is a fixed
    // `HISTORY_HEIGHT` of 2 inner rows at every breakpoint — enough for RX and TX
    // but never a third row, so its caret note is computed but never has a row to
    // render into. The Network screen's history panel is sized from what is left
    // over instead, so at 80 columns it is the narrowest place a caret note
    // actually reaches the screen — the binding case for §2.5's comparisons
    // fitting beside it.
    let mut state = Fixture::new((80, 24), ViewId::Network).build();
    for _ in 0..4 {
        let _ = monitrs_tui::app::reduce(
            &mut state,
            monitrs_tui::action::Action::SeekHistory(monitrs_tui::action::Seek::Backward(1)),
        );
    }
    let text = text_of(&frame(&state, ascii()));
    let caret_row = text
        .lines()
        .find(|line| line.contains('^'))
        .expect("the caret row must be present at 80 columns");
    // Every row `text_of` prints is wrapped in one synthetic `|` marker on each
    // end (not part of the rendered buffer), so the true rendered width is the
    // state's own column count.
    let (width, _) = state.size();
    assert_eq!(
        monitrs_core::units::display_width(caret_row),
        usize::from(width) + 2,
        "the caret row must fill its 80-column panel without wrapping or being \
         cut short: {caret_row:?}"
    );
    assert!(
        caret_row.contains("cpu prev"),
        "the comparison must reach the caret at 80 columns too: {caret_row:?}"
    );
    assert!(
        !caret_row.trim_end_matches('|').trim_end().is_empty(),
        "a note that does not fit must never render as a blank row: {caret_row:?}"
    );
}
