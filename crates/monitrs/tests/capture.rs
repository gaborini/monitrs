//! Captures real frames from the live collector, and measures what §16.1 budgets.
//!
//! Every test here is `#[ignore]`d because each one reads the live system: they are
//! evidence-gathering runs, not assertions about a fixture. Run them with
//! `cargo test --test capture -- --ignored --nocapture`.
//!
//! Two jobs, both of which need the *real* rendering path rather than a description
//! of it:
//!
//! * **Frames for the documentation.** §20.1 forbids a fabricated screenshot, so
//!   the frames in `README.md` are written by
//!   [`capture_real_frames_for_the_documentation`] straight out of
//!   `views::render` with live data. Using ratatui's `TestBackend` rather than a
//!   terminal capture matters: the buffer is exact, where reconstructing a real
//!   terminal's differential updates is guesswork.
//! * **The frame-time and input-latency budgets of §16.1.** Those are the two
//!   budgets that need the assembled renderer, and they are measured here with the
//!   live collector rather than with the fake, because the fake's cost is not the
//!   thing being budgeted.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              failure must name the line that broke"
)]

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use monitrs_collectors::{DueTiers, SampleTick, SnapshotSource, platform_collector};
use monitrs_core::diagnostics::{PressureEngine, Thresholds};
use monitrs_core::process::{ProcessSort, ProcessSortKey};
use monitrs_core::units::ByteUnits;
use monitrs_tui::action::ViewId;
use monitrs_tui::app::{AppSettings, AppState, DisplaySettings, apply};
use monitrs_tui::event::{Event, Key, KeyPress, TerminalEvent};
use monitrs_tui::glyphs::{GlyphMode, TerminalEnv};
use monitrs_tui::theme::{ColorMode, ThemeId};
use monitrs_tui::views;
use monitrs_tui::widgets::Presentation;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Where captured frames are written for the documentation to include.
const FRAME_DIR: &str = "../../docs/screenshots";

/// The §16.1 reference frame size for the render budget.
const WIDE: (u16, u16) = (160, 48);

/// Samples a captured frame is warmed with, so its radar is not all `warming`.
///
/// The default pressure policy wants 10 of the last 15 samples before it commits a
/// state (§11.3), so this is that window plus a little slack. A three-sample frame
/// is honest and useless as documentation: every radar row reads `warming`.
const RADAR_SAMPLES: usize = 16;

/// A state fed by the live collector, with presentation fixed by the caller.
struct Live {
    state: AppState,
    // The boxed platform source, not the bare baseline: a frame captured from
    // `CommonCollector` would document a program nobody runs (§9.2).
    collector: Box<dyn SnapshotSource>,
    pressure: PressureEngine,
    tick: SampleTick,
    /// How many samples have been taken, so the first tick keeps its zero interval.
    taken: usize,
}

impl Live {
    fn new(size: (u16, u16), glyphs: GlyphMode, color: ColorMode, theme: ThemeId) -> Self {
        let collector = platform_collector().expect("the platform collector must construct");
        let started = Instant::now();
        let state = AppState::new(AppSettings {
            started_at: started,
            size,
            view: ViewId::Overview,
            sort: ProcessSort::descending(ProcessSortKey::Cpu),
            hide_kernel_threads: true,
            display: DisplaySettings {
                theme,
                glyph_mode: glyphs,
                color_mode: color,
                color_explicit: true,
                byte_units: ByteUnits::Iec,
            },
            // An empty environment, so glyph and colour resolution comes from the
            // explicit settings above rather than from whoever runs the test.
            env: TerminalEnv::empty(),
            sample_interval: Duration::from_millis(250),
            ..AppSettings::default()
        });
        Self {
            state,
            collector,
            pressure: PressureEngine::new(Thresholds::default()),
            tick: SampleTick::first(started, SystemTime::now()),
            taken: 0,
        }
    }

    /// Collects one sample and feeds it through the real reducer.
    ///
    /// The first tick is used as-is, so its `elapsed` is `ZERO` and every rate is
    /// `WarmingUp` — which is what the sampler thread does, and what §8.2 requires.
    /// Advancing it immediately would hand the pressure engine a sub-microsecond
    /// interval as its first observation.
    fn sample(&mut self) {
        if self.taken > 0 {
            self.tick = self
                .tick
                .advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        }
        self.taken += 1;
        let mut snapshot = self.collector.sample(&self.tick).expect("a live sample");
        // The sampler thread does this in the real program; doing it here is what
        // makes the captured radar the one a user sees.
        snapshot.pressure = self.pressure.observe(&snapshot);
        let _ = apply(
            &mut self.state,
            Event::<()>::Snapshot(std::sync::Arc::new(snapshot)),
        );
    }

    /// Collects `count` samples, spaced far enough apart to leave `warming up`.
    fn warm(&mut self, count: usize) {
        for index in 0..count {
            if index > 0 {
                std::thread::sleep(sysinfo_min_interval());
            }
            self.sample();
        }
    }

    fn press(&mut self, key: KeyPress) {
        let _ = apply(
            &mut self.state,
            Event::<()>::Terminal(TerminalEvent::Key(key)),
        );
    }

    /// Renders one frame and returns it as text plus how long the draw took.
    fn render(&mut self, size: (u16, u16)) -> (String, Duration) {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1))
            .expect("a test backend never fails to initialise");
        let state = &self.state;
        let started = Instant::now();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let presentation =
                    Presentation::new(state.glyph_set(), state.theme(), state.color_depth())
                        .with_units(state.display().byte_units);
                views::render(frame, area, state, presentation);
            })
            .expect("drawing to a test backend never fails");
        let elapsed = started.elapsed();
        (buffer_text(terminal.backend().buffer()), elapsed)
    }
}

/// The gap `sysinfo` needs between CPU reads for a real delta.
fn sysinfo_min_interval() -> Duration {
    Duration::from_millis(300)
}

/// A buffer as plain text, one line per row, trailing blanks trimmed.
fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The identifiers a published frame should not carry.
///
/// These frames go into a public repository, and a system monitor's frame shows the
/// machine it was taken on: the hostname in the header, the owner of every row, and
/// their home directory in a hundred command lines. §19 keeps a user's own data out
/// of what monitrs writes down, and the same courtesy applies to what the project
/// publishes about whoever built it.
///
/// A *substitution*, not a redaction of measurements: every number, every process
/// name, every state and every column stays exactly as the renderer drew it, which
/// is what §20.1 is about. Where a substitute is a different length the frame's
/// alignment shifts, and that is left visible rather than padded back into a shape
/// the renderer never produced.
///
/// Anyone who would rather publish their own hostname can read the frames off their
/// terminal instead; nothing here is needed to render them.
#[derive(Debug)]
struct Anonymize {
    /// The hostname exactly as the snapshot reported it, so there is no guessing
    /// about `.local` suffixes or which of `HOST`/`HOSTNAME` the shell set.
    host: Option<String>,
    user: Option<String>,
}

impl Anonymize {
    /// Reads the hostname from the state's own snapshot.
    fn for_state(state: &AppState) -> Self {
        Self {
            host: state
                .snapshot()
                .and_then(|snapshot| snapshot.host.hostname.fresh().cloned())
                .map(|hostname| hostname.into_string()),
            user: std::env::var("USER").ok().filter(|user| !user.is_empty()),
        }
    }

    fn apply(&self, text: &str) -> String {
        let mut out = text.to_owned();
        if let Some(host) = &self.host {
            out = out.replace(host.as_str(), "dev-mbp");
        }
        if let Some(user) = &self.user {
            out = out.replace(&format!("/Users/{user}"), "/Users/you");
            out = out.replace(&format!("/home/{user}"), "/home/you");
            // The `USER` column is a padded field. Substituting only whole fields
            // keeps a two-letter login from rewriting the middle of a process name,
            // and the substitute is chosen to be exactly as wide as the login it
            // replaces so that every column to its right stays where the renderer put
            // it. A frame whose alignment is off by one would look like a layout bug
            // in the thing it is documenting.
            let width = user.chars().count();
            let stand_in = match width {
                0 => "",
                1 => "u",
                2 => "me",
                _ => "you",
            };
            out = out.replace(&format!(" {user} "), &format!(" {stand_in:<width$} "));
        }
        out
    }
}

fn write_frame(name: &str, text: &str, anonymize: &Anonymize) {
    let dir = Path::new(FRAME_DIR);
    std::fs::create_dir_all(dir).expect("the screenshot directory must be creatable");
    let path = dir.join(name);
    let published = anonymize.apply(text);
    std::fs::write(&path, &published).expect("the frame must be writable");
    println!("wrote {} ({} bytes)", path.display(), published.len());
}

#[test]
#[ignore = "capture run: reads the live system and writes into docs/screenshots"]
fn capture_real_frames_for_the_documentation() {
    // ASCII and no colour, because that is the form that survives a README, a
    // terminal without colour, and a screen reader (§5.1, §5.2).
    let mut live = Live::new(WIDE, GlyphMode::Ascii, ColorMode::Off, ThemeId::DefaultDark);
    live.warm(RADAR_SAMPLES);
    let anonymize = Anonymize::for_state(&live.state);

    let (overview, _) = live.render(WIDE);
    assert!(
        overview.contains("PROCESSES"),
        "the overview must have a process panel"
    );
    write_frame("overview-ascii.txt", &overview, &anonymize);

    // The Processes screen at the §5.7 compact floor, which is where the column
    // priority actually has to make a decision.
    live.press(KeyPress::char('2'));
    let (compact, _) = live.render((80, 24));
    write_frame("processes-80x24-ascii.txt", &compact, &anonymize);

    live.press(KeyPress::char('5'));
    let (inspect, _) = live.render(WIDE);
    assert!(
        inspect.contains("n/a") || inspect.contains("unsupported"),
        "Inspect must name what this machine cannot report"
    );
    write_frame("inspect-ascii.txt", &inspect, &anonymize);

    // And one Unicode frame, so the enhanced mode is documented too.
    let mut fancy = Live::new(
        WIDE,
        GlyphMode::Unicode,
        ColorMode::Off,
        ThemeId::DefaultDark,
    );
    fancy.warm(RADAR_SAMPLES);
    let anonymize = Anonymize::for_state(&fancy.state);
    let (unicode, _) = fancy.render(WIDE);
    write_frame("overview-unicode.txt", &unicode, &anonymize);
}

#[test]
#[ignore = "measurement run: reads the live system"]
fn measure_the_frame_render_budget() {
    // §16.1: an ordinary frame render must be below 16 ms at 160x48.
    let mut live = Live::new(
        WIDE,
        GlyphMode::Unicode,
        ColorMode::TrueColor,
        ThemeId::DefaultDark,
    );
    live.warm(3);

    let mut samples = Vec::new();
    for view in ['1', '2', '3', '4', '5'] {
        live.press(KeyPress::char(view));
        // Discard the first draw per view: it pays for lazily built layout caches,
        // and §16.1 budgets the *ordinary* frame.
        let _ = live.render(WIDE);
        for _ in 0..40 {
            let (_, elapsed) = live.render(WIDE);
            samples.push(elapsed);
        }
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let p95 = samples[samples.len() * 95 / 100];
    let worst = samples.last().copied().unwrap_or_default();
    let processes = live.state.snapshot().map_or(0, |s| s.process_count());

    println!(
        "frame render at 160x48 over {} draws, {} processes: median {:?}, p95 {:?}, max {:?}",
        samples.len(),
        processes,
        median,
        p95,
        worst
    );
    assert!(
        p95 < Duration::from_millis(16),
        "§16.1 budgets an ordinary frame below 16ms at 160x48; p95 was {p95:?}"
    );
}

#[test]
#[ignore = "measurement run: reads the live system"]
fn measure_the_input_to_visible_response_budget() {
    // §16.1: input to visible response below 50 ms when no collector result is
    // needed. That is exactly reduce-then-render, which is what this measures.
    let mut live = Live::new(
        WIDE,
        GlyphMode::Unicode,
        ColorMode::TrueColor,
        ThemeId::DefaultDark,
    );
    live.warm(3);

    let keys = [
        KeyPress::char('j'),
        KeyPress::char('k'),
        KeyPress::char('2'),
        KeyPress::char('f'),
        KeyPress::char('s'),
        KeyPress::plain(Key::Escape),
        KeyPress::char('1'),
        KeyPress::char('?'),
        KeyPress::plain(Key::Escape),
    ];
    let mut samples = Vec::new();
    for _ in 0..20 {
        for key in keys {
            let started = Instant::now();
            live.press(key);
            let _ = live.render(WIDE);
            samples.push(started.elapsed());
        }
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let p95 = samples[samples.len() * 95 / 100];

    println!(
        "input to visible response over {} keypresses: median {:?}, p95 {:?}",
        samples.len(),
        median,
        p95
    );
    assert!(
        p95 < Duration::from_millis(50),
        "§16.1 budgets input-to-visible-response below 50ms; p95 was {p95:?}"
    );
}

#[test]
#[ignore = "measurement run: reads the live system"]
fn measure_the_sample_collection_budget_per_tick_shape() {
    // §16.1 budgets "sample collection below 200 ms p95". Which tick that is about
    // matters more than the number: at the default intervals four ticks in five are
    // fast-only, the fifth adds the medium tier, and one in thirty adds the slow tier.
    // Measuring only `DueTiers::ALL` — which is all an out-of-crate test could
    // construct before `fast_only` existed — reports the most expensive tick there is
    // and makes the collector look four times worse than it usually is.
    let mut collector = platform_collector().expect("constructs");
    let started = Instant::now();
    let mut tick = SampleTick::first(started, SystemTime::now());
    // Two full samples first, so nothing below pays a first-read cost.
    for _ in 0..2 {
        let _ = collector.sample(&tick).expect("a live sample");
        std::thread::sleep(Duration::from_millis(250));
        tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
    }

    let mut processes = 0usize;
    for (label, due) in [
        ("fast only (4 ticks in 5)", DueTiers::fast_only()),
        ("fast + medium (every 5th)", DueTiers::fast_and_medium()),
        ("every tier (every 30th, and the first)", DueTiers::ALL),
    ] {
        let mut samples = Vec::new();
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(250));
            tick = tick.advance(Instant::now(), SystemTime::now(), due);
            let at = Instant::now();
            let snapshot = collector.sample(&tick).expect("a live sample");
            samples.push(at.elapsed());
            processes = snapshot.process_count();
        }
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        let p95 = samples[samples.len() * 95 / 100];
        println!("collection, {label}: median {median:?}, p95 {p95:?}");
        assert!(
            p95 < Duration::from_millis(200),
            "§16.1 budgets collection below 200ms p95; {label} was {p95:?}"
        );
    }
    println!("  measured against {processes} processes");
}
