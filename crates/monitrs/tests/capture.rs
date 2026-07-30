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
use monitrs_core::model::ProcessIdentity;
use monitrs_core::process::{ProcessSort, ProcessSortKey, SubtreeUsage};
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

    /// Types `line` into the command palette and submits it (§6.3).
    ///
    /// Through the overlay rather than around it, because that is the only way a user can
    /// reach a palette command — and because it means the parser and the submit path are
    /// under test here too, not just the action they produce.
    fn command(&mut self, line: &str) {
        self.press(KeyPress::char(':'));
        for character in line.chars() {
            self.press(KeyPress::char(character));
        }
        self.press(KeyPress::plain(Key::Enter));
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

/// Whether a §16.1 budget may be *asserted* on this machine, or only reported.
///
/// §16.1's own last line is that these are "engineering budgets, not marketing claims,
/// until measured reproducibly", and a shared CI runner is not reproducible: it is
/// virtualised, co-tenanted, and its p95 includes a hypervisor's scheduling. Asserting
/// a 16 ms frame budget there produced exactly what you would expect — a failure at
/// 17.4 ms on hardware where this machine measures 0.4 ms, with no defect behind it.
///
/// So the budget is asserted only when someone says the machine is worth quoting, by
/// setting `MONITRS_REFERENCE_MACHINE=1`. That is the run whose numbers go into
/// `docs/benchmarks.md`, alongside the machine and the command, as §16.3 requires.
///
/// Every run still asserts [`REGRESSION_CEILING`], which is generous enough that no
/// scheduler can trip it and tight enough that an order-of-magnitude regression cannot
/// hide. That is the part CI is good for.
fn budgets_are_assertable() -> bool {
    std::env::var("MONITRS_REFERENCE_MACHINE").is_ok_and(|value| value == "1")
}

/// The factor by which a measurement may exceed its §16.1 budget before it is a defect
/// rather than a busy machine.
///
/// Twelve, which on the tightest budget here — 16 ms for a frame — is 192 ms. The
/// observed p95 on a developer machine is 0.4 ms and on a CI runner 17 ms, so this
/// catches a real regression by two orders of magnitude while leaving both alone.
const REGRESSION_CEILING: u32 = 12;

/// Reports a measurement against its budget, asserting what the machine can support.
fn check_budget(label: &str, measured: Duration, budget: Duration) {
    let ceiling = budget.saturating_mul(REGRESSION_CEILING);
    assert!(
        measured < ceiling,
        "{label}: {measured:?} is more than {REGRESSION_CEILING}x the §16.1 budget of \
         {budget:?}. That is not a busy machine, it is a regression."
    );
    if budgets_are_assertable() {
        assert!(
            measured < budget,
            "{label}: {measured:?} exceeds the §16.1 budget of {budget:?} on a machine \
             declared as a reference machine"
        );
    } else if measured >= budget {
        println!(
            "  note: {label} is over its {budget:?} budget at {measured:?}. Set \
             MONITRS_REFERENCE_MACHINE=1 on a machine you control to assert it."
        );
    }
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

    // The CPU screen, which is the whole reason `3` exists.
    live.press(KeyPress::char('3'));
    let (cpu, _) = live.render(WIDE);
    assert!(
        cpu.contains("BUSIEST PROCESSES"),
        "the CPU screen must name the processes accounting for the load"
    );
    write_frame("cpu-ascii.txt", &cpu, &anonymize);

    live.press(KeyPress::char('6'));
    let (inspect, _) = live.render(WIDE);
    assert!(
        inspect.contains("n/a") || inspect.contains("unsupported"),
        "Inspect must name what this machine cannot report"
    );
    write_frame("inspect-ascii.txt", &inspect, &anonymize);

    // The two screens added after 0.1.0. The README says the full frames live in
    // `docs/screenshots/`, so a screen with no captured frame makes that sentence false —
    // and §20.1 allows no other kind of screenshot than one written from the renderer.
    live.press(KeyPress::char('4'));
    let (storage, _) = live.render(WIDE);
    assert!(
        storage.contains("TOP DISK I/O"),
        "the Storage screen must name the processes doing the I/O"
    );
    write_frame("storage-ascii.txt", &storage, &anonymize);

    live.press(KeyPress::char('7'));
    let (battery, _) = live.render(WIDE);
    assert!(
        battery.contains("THERMAL SENSORS"),
        "the Battery screen must carry the thermal sensors"
    );
    write_frame("battery-ascii.txt", &battery, &anonymize);

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
#[ignore = "live-system run: follows a real process tree and reads the frame back"]
fn following_a_real_process_tree_scopes_the_real_table() {
    // The §7.2 table is where a defect hides best: the reducer tests use a fixture with
    // eight processes and one obvious family, while this machine has a thousand processes,
    // several deep trees, and PIDs that are reused. Every serious defect in this project
    // was found by rendering a real frame, so this presses the real key against the real
    // machine and reads the answer off the screen.
    let mut live = Live::new(WIDE, GlyphMode::Ascii, ColorMode::Off, ThemeId::DefaultDark);
    // Two samples, because a process's CPU is `WarmingUp` until it has a delta — and a
    // subtree summed from `WarmingUp` members is the case that must *not* read as zero.
    live.warm(2);
    live.press(KeyPress::char('2'));

    // Skipped rather than failed on a machine with no family to follow. A runner whose
    // process tree is flat says nothing about this feature, and a test that panics there
    // would be reporting the shape of the runner as a defect in monitrs.
    let Some(root) = busiest_parent(&live.state) else {
        println!("no process on this machine has a non-kernel child; nothing to follow");
        return;
    };
    let expected = subtree_pids(&live.state, root);
    if expected.len() <= 1 {
        println!(
            "{}'s family shrank to itself between samples; skipping",
            root.pid
        );
        return;
    }

    // The precondition, stated against the machine rather than against the frame: the
    // family has to be a *part* of this host for scoping to it to be observable. Comparing
    // it to the number of rendered rows instead is how the first version of this test
    // became flaky — the panel is 40 rows tall, so on a run where the busiest family had
    // 40 members the "unscoped table is larger" check failed while the feature worked.
    let population = live.state.snapshot().expect("a snapshot").processes.len();
    assert!(
        expected.len() < population,
        "{} members out of {population} processes leaves nothing to scope away",
        expected.len()
    );

    let (before, _) = live.render(WIDE);
    let shown_before = shown_count(&before).expect("the panel states its row count");

    // The palette rather than `F`, because selecting a specific row out of a thousand
    // would be a test of the arrow keys. `follow <pid>` goes through the same reducer.
    live.command(&format!("follow {}", root.pid));
    let (after, _) = live.render(WIDE);

    // Asserted against the panel's *own* line, not against the frame. Two of these read
    // the whole frame at first and passed for the wrong reason: `cpu ` and `rss ` also
    // appear in the header's `8 cpu  8 core`, and `following` appeared in this very test
    // binary's command line as rendered in the process table — `-- --ignored following`.
    // A frame-wide substring search is not an assertion about a panel.
    let title = panel_title(&after).expect("the process panel is on screen");
    assert!(
        title.contains(&format!("following {}", root.pid)),
        "the title must say why the table is short:\n{title}"
    );
    assert!(
        title.contains("cpu ") && title.contains("rss "),
        "the label must carry the family's summed cost, which is on no row:\n{title}"
    );

    let visible_after = table_pids(&after);
    assert!(
        !visible_after.is_empty(),
        "a followed subtree must not empty the table — if every member is filtered out, \
         the root was the wrong choice rather than the scope being wrong:\n{}",
        first_lines(&after, 12)
    );
    for pid in &visible_after {
        assert!(
            expected.contains(pid),
            "{pid} is on screen but is not in {}'s subtree {expected:?}",
            root.pid
        );
    }
    // Asked of the scoped row list rather than of the drawn rows: following a process must
    // not drop the process itself, which is a claim about the scope and not about what fits.
    // The panel is 40 rows tall and this family has 40 members, so on a run where the root
    // sorts last by CPU it is legitimately below the fold — which is how the frame-based
    // version of this assertion failed one run in six while the feature was correct.
    assert!(
        live.state.rows().row_of(root).is_some(),
        "the followed root must be in the scoped table"
    );
    // A subtree of a live machine loses and gains members between samples, so the rows are
    // asserted to be a *subset* of the membership rather than equal to it. The count comes
    // from the panel's own label rather than from the rendered rows, because the rows stop
    // at the bottom of the panel and the label does not.
    let shown_after = shown_count(&after).expect("the panel states its row count");
    assert!(
        shown_after < shown_before,
        "the scope must actually remove rows: {shown_before} -> {shown_after}"
    );

    // Printed, because a live-system test whose only output is `ok` cannot show that it
    // looked at something real — and because the label is the deliverable here.
    println!("{}", panel_title(&after).unwrap_or("no panel").trim());
    println!(
        "followed {} with {} members; the panel went from {shown_before} rows to \
         {shown_after}, of which {} were drawn",
        root.pid,
        expected.len(),
        visible_after.len()
    );

    // The sums cover the whole family, not the rows on screen, so narrowing the view has
    // to make the family's size explicit — otherwise `cpu 10%` reads as belonging to the
    // handful of rows a filter left behind.
    live.command("filter zzzzzz-no-such-process");
    let (narrowed, _) = live.render(WIDE);
    let narrowed_title = panel_title(&narrowed).expect("the process panel is on screen");
    assert!(
        narrowed_title.contains(&format!("subtree of {}", expected.len())),
        "a filtered view must state what the sums are over:\n{narrowed_title}"
    );
    println!("{}", panel_title(&narrowed).unwrap_or("no panel").trim());
    live.command("filter");

    live.command("unfollow");
    let (lifted, _) = live.render(WIDE);
    let lifted_title = panel_title(&lifted).expect("the process panel is on screen");
    assert!(
        !lifted_title.contains("following"),
        "the scope must be gone from the title:\n{lifted_title}"
    );
    assert!(
        shown_count(&lifted).expect("the panel states its row count") > shown_after,
        "and the rest of the machine must come back"
    );
}

/// The process panel's own line: its title, its state, and its trailing label.
///
/// Everything this test claims about the panel is claimed about this one line. Searching
/// the whole frame instead is how three of its assertions first went wrong — the header
/// says `8 cpu  8 core`, and the process table renders this test binary's own command
/// line, so `cpu `, `rss ` and `following` all appear on a frame that proves none of them.
fn panel_title(frame: &str) -> Option<&str> {
    frame
        .lines()
        .find(|line| line.contains("PROCESSES  sort"))
        .map(|line| line.trim_matches('|'))
}

/// The number of rows the process panel says it is showing, from its trailing label.
///
/// `12 of 218 total` gives 12, `218 total` gives 218. Read from the label rather than
/// counted off the screen because the rows stop where the panel ends: a family with more
/// members than the panel is tall would otherwise look like no reduction at all.
fn shown_count(frame: &str) -> Option<usize> {
    let title = panel_title(frame)?;
    // Everything before ` total`, so a trailing `, cpu 5.9%, rss 9.9G` cannot be read as
    // a count.
    let label = title.rsplit_once(" total")?.0;
    // `12 of 218` means twelve are shown; a bare `218` means all of them are.
    let shown = match label.rsplit_once(" of ") {
        Some((head, _)) => head,
        None => label,
    };
    shown.split_whitespace().last()?.parse().ok()
}

/// The process with the most children the table would actually draw, if any.
///
/// Chosen by child count rather than by PID so the test picks a real family — on macOS
/// PID 1 parents nearly everything, which would make the "scope removes rows" assertion
/// pass for the wrong reason.
///
/// **Kernel threads are excluded, as candidates and as children**, and that is not a
/// detail. On a Linux runner the process with by far the most children is `kthreadd`, and
/// this harness hides kernel threads the way the shipped default does — so following
/// `kthreadd` would scope the table to two hundred rows that are all filtered out, and the
/// test would fail on an empty table while the feature worked perfectly. The root has to be
/// a family the table can show.
fn busiest_parent(state: &AppState) -> Option<ProcessIdentity> {
    let snapshot = state.snapshot()?;
    let mut best: Option<(usize, ProcessIdentity)> = None;
    for process in &snapshot.processes {
        // Skip PID 1 and anything parented by nothing: a subtree of init is the machine.
        if process.identity.pid <= 1 || process.is_kernel_thread {
            continue;
        }
        let children = snapshot
            .processes
            .iter()
            .filter(|other| {
                other.parent_pid == Some(process.identity.pid) && !other.is_kernel_thread
            })
            .count();
        if children == 0 {
            continue;
        }
        if best.is_none_or(|(most, _)| children > most) {
            best = Some((children, process.identity));
        }
    }
    best.map(|(_, identity)| identity)
}

/// Every PID in `root`'s subtree, according to the aggregation under test.
fn subtree_pids(state: &AppState, root: ProcessIdentity) -> Vec<u32> {
    let snapshot = state.snapshot().expect("a snapshot");
    SubtreeUsage::of(snapshot, root)
        .map(|usage| usage.members.iter().map(|member| member.pid).collect())
        .unwrap_or_default()
}

/// The PIDs of the process table's rows in a rendered frame.
///
/// Reads the frame rather than the state on purpose: the question this test exists to
/// answer is what reached the screen.
fn table_pids(frame: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut inside = false;
    for line in frame.lines() {
        if line.contains("PROCESSES  sort") {
            inside = true;
            continue;
        }
        if inside && line.starts_with('+') {
            break;
        }
        if !inside {
            continue;
        }
        // `|`, the selection marker, the pin marker, then the right-aligned PID.
        let cells = line.trim_start_matches('|');
        let Some(field) = cells.get(..12) else {
            continue;
        };
        if let Some(pid) = field
            .split_whitespace()
            .next()
            .and_then(|word| word.parse::<u32>().ok())
        {
            pids.push(pid);
        }
    }
    pids
}

fn first_lines(frame: &str, count: usize) -> String {
    frame.lines().take(count).collect::<Vec<_>>().join("\n")
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
    check_budget("frame render p95 at 160x48", p95, Duration::from_millis(16));
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
    check_budget(
        "input-to-visible-response p95",
        p95,
        Duration::from_millis(50),
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
        check_budget(
            &format!("collection p95, {label}"),
            p95,
            Duration::from_millis(200),
        );
    }
    println!("  measured against {processes} processes");
}
