//! The §21 M6 accessibility review, as executable assertions.
//!
//! `docs/accessibility.md` is the prose half of this file. Everything the document
//! claims about symbols, colour depths, contrast ratios, ASCII purity, animation,
//! and the narrow breakpoints is asserted here against the real renderer, so the
//! document cannot quietly become out of date.
//!
//! Five properties are pinned:
//!
//! 1. **Colour is never the only signal** (§5.2, §2.3). Every state enum's symbol
//!    is distinct *within its type*, and the widgets that colour by state actually
//!    put the symbol on screen.
//! 2. **`--color off` stays legible.** A frame is rendered at all four depths; the
//!    characters must be identical and the no-colour frame must still distinguish
//!    the selection, the notable process states, and pressure severity.
//! 3. **Contrast.** WCAG 2.1 relative-luminance ratios for every built-in theme at
//!    every colour depth, checked against a floor of 4.5:1 for text and 3:1 for
//!    symbols and bands. `contrast_report_for_every_theme_and_depth` prints the
//!    table that `docs/accessibility.md` publishes; run it with `--nocapture` to
//!    regenerate the numbers.
//! 4. **Strict ASCII is genuinely 7-bit** — over whole frames of every screen and
//!    every fixture, and over every state string `monitrs-core` and the keymap can
//!    produce, not only over the [`Glyph`] table.
//! 5. **Nothing animates** (§3.2, §5.2). No frame carries a blink attribute and the
//!    same state renders byte-identically twice.
//!
//! [`Glyph`]: monitrs_tui::glyphs::Glyph

// An integration test is its own crate, so the library's `cfg(test)` allowance
// does not reach here. `expect` is how a test asserts a precondition (§18.2).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use core::time::Duration;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use monitrs_collectors::fake::{FakeCollector, FakeProcess, Pattern, Scenario};
use monitrs_collectors::source::{SampleTick, SnapshotSource};
use monitrs_collectors::tier::DueTiers;
use monitrs_core::diagnostics::{PressureEngine, Thresholds};
use monitrs_core::model::{
    CapabilityState, ChargeState, LinkState, MetricState, PressureState, ProcessState, Severity,
    UnavailableReason,
};
use monitrs_core::process::{ProcessSort, ProcessSortKey};
use monitrs_core::units::display_width;
use monitrs_tui::action::ViewId;
use monitrs_tui::app::{AppSettings, AppState};
use monitrs_tui::event::Event;
use monitrs_tui::glyphs::{Glyph, GlyphSet};
use monitrs_tui::keymap::{InputMode, Keymap};
use monitrs_tui::layout::{Breakpoint, Column};
use monitrs_tui::theme::{ColorDepth, Theme, ThemeId, Token};
use monitrs_tui::views;
use monitrs_tui::widgets::{Presentation, states};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

// ---------------------------------------------------------------------------
// Rendering harness
// ---------------------------------------------------------------------------

/// Builds an [`AppState`] the way the runtime does: fake snapshots through the
/// real reducer, with the Pressure Radar derived by the real engine.
///
/// Hand-built snapshots are deliberately avoided (§17.5): a fixture that cannot
/// come out of a collector proves nothing about what a user will see.
fn state_of(scenario: Scenario, samples: u64, size: (u16, u16), view: ViewId) -> AppState {
    let mut state = AppState::new(AppSettings {
        started_at: Instant::now(),
        size,
        view,
        sort: ProcessSort::ascending(ProcessSortKey::Pid),
        ..AppSettings::default()
    });
    let mut collector = FakeCollector::new(scenario);
    let mut engine = PressureEngine::new(Thresholds::default());
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
    for index in 0..samples {
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
        snapshot.pressure = engine.observe(&snapshot);
        let _ = monitrs_tui::app::apply(&mut state, Event::<()>::Snapshot(Arc::new(snapshot)));
    }
    state
}

/// The reference fixture: 24 samples of the §5.5 scenario on a wide terminal.
fn reference(view: ViewId) -> AppState {
    state_of(Scenario::default(), 24, (140, 38), view)
}

/// A scenario whose CPU and memory are pinned above the §12 critical thresholds,
/// so the Pressure Radar really reaches `critical` through the engine's own
/// hysteresis rather than through a hand-set state.
fn saturated_scenario() -> Scenario {
    Scenario {
        cpu: Pattern::Steady(99.0),
        memory: Pattern::Steady(99.0),
        ..Scenario::default()
    }
}

/// A scenario with the zombie and uninterruptible-sleep rows §7.2 singles out.
fn notable_scenario() -> Scenario {
    Scenario {
        processes: vec![
            FakeProcess::new(1, 1, "launchd", "/sbin/launchd").with_cpu(Pattern::Steady(0.1)),
            FakeProcess::new(1_221, 700_050, "postgres", "postgres: checkpointer")
                .with_cpu(Pattern::Steady(0.0))
                .with_state(ProcessState::Zombie)
                .with_user("postgres", 70),
            FakeProcess::new(4_410, 660_000, "nfsd", "nfsd")
                .with_cpu(Pattern::Steady(0.0))
                .with_state(ProcessState::UninterruptibleSleep)
                .with_user("root", 0),
        ],
        ..Scenario::default()
    }
}

/// Every fixture the review renders, with a name for failure messages.
fn fixtures() -> Vec<(&'static str, Scenario, u64)> {
    vec![
        ("reference", Scenario::default(), 24),
        ("first-frame", Scenario::default(), 1),
        ("no-sample", Scenario::default(), 0),
        ("permission-denied", Scenario::permission_denied(), 24),
        ("notable-states", notable_scenario(), 24),
        ("saturated", saturated_scenario(), 24),
        (
            "stale",
            Scenario {
                stale_from: Some(6),
                ..Scenario::default()
            },
            24,
        ),
        ("empty", Scenario::empty(), 24),
        ("many-cores", Scenario::many_cores(), 24),
        ("containerised", Scenario::containerised(), 24),
    ]
}

/// The presentation used for the plain-ASCII, full-colour baseline.
fn ascii(theme: &Theme, depth: ColorDepth) -> Presentation<'_> {
    Presentation::new(GlyphSet::ascii(), theme, depth)
}

/// Renders one whole frame and returns the buffer.
fn frame(state: &AppState, presentation: Presentation<'_>) -> Buffer {
    let (width, height) = state.size();
    let mut terminal = Terminal::new(TestBackend::new(width.max(1), height.max(1)))
        .expect("a test backend never fails to initialise");
    terminal
        .draw(|frame| {
            views::render(frame, Rect::new(0, 0, width, height), state, presentation);
        })
        .expect("drawing to a test backend never fails");
    terminal.backend().buffer().clone()
}

/// The characters of a buffer, one line per row, continuation cells skipped.
fn text_of(buffer: &Buffer) -> String {
    let mut out = String::new();
    for y in buffer.area.top()..buffer.area.bottom() {
        let mut skip = 0u16;
        for x in buffer.area.left()..buffer.area.right() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if let Some(cell) = buffer.cell((x, y)) {
                let symbol = cell.symbol();
                out.push_str(symbol);
                skip = u16::try_from(display_width(symbol))
                    .unwrap_or(1)
                    .saturating_sub(1);
            }
        }
        out.push('\n');
    }
    out
}

/// Every screen, so a claim is never checked on one view only.
const VIEWS: [ViewId; 5] = [
    ViewId::Overview,
    ViewId::Processes,
    ViewId::Storage,
    ViewId::Network,
    ViewId::Inspect,
];

/// The four §5.7 bands, by a representative size each.
const SIZES: [(u16, u16); 5] = [(140, 38), (110, 30), (80, 24), (60, 16), (52, 12)];

// ---------------------------------------------------------------------------
// 1. Colour is never the only signal (§2.3, §5.2)
// ---------------------------------------------------------------------------

/// Every [`MetricState`] variant, guarded by an exhaustive match.
///
/// The match is the completeness guard: adding a variant to `MetricState` fails
/// to compile here, which is the prompt to extend the list. The same idiom is used
/// for the five other state enums below, and it is the one `Token::index` and
/// `Glyph::index` already use inside the crate.
fn metric_states() -> Vec<MetricState<u64>> {
    let all = vec![
        MetricState::Available(1),
        MetricState::Stale {
            value: 1,
            age: Duration::from_secs(2),
        },
        MetricState::WarmingUp,
        MetricState::PermissionDenied,
        MetricState::Unsupported,
        MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
    ];
    for state in &all {
        let _guard: u8 = match state {
            MetricState::Available(_) => 0,
            MetricState::Stale { .. } => 1,
            MetricState::WarmingUp => 2,
            MetricState::PermissionDenied => 3,
            MetricState::Unsupported => 4,
            MetricState::TemporarilyUnavailable(_) => 5,
        };
    }
    all
}

/// Every [`PressureState`], guarded by an exhaustive match.
fn pressure_states() -> Vec<PressureState> {
    let all = vec![
        PressureState::Normal,
        PressureState::Watch,
        PressureState::Critical,
    ];
    for state in &all {
        let _guard: u8 = match state {
            PressureState::Normal => 0,
            PressureState::Watch => 1,
            PressureState::Critical => 2,
        };
    }
    all
}

/// Every [`ProcessState`], guarded by an exhaustive match.
fn process_states() -> Vec<ProcessState> {
    let all = vec![
        ProcessState::Running,
        ProcessState::Sleeping,
        ProcessState::UninterruptibleSleep,
        ProcessState::Zombie,
        ProcessState::Stopped,
        ProcessState::Traced,
        ProcessState::Idle,
        ProcessState::Dead,
        ProcessState::Unknown,
    ];
    for state in &all {
        let _guard: u8 = match state {
            ProcessState::Running => 0,
            ProcessState::Sleeping => 1,
            ProcessState::UninterruptibleSleep => 2,
            ProcessState::Zombie => 3,
            ProcessState::Stopped => 4,
            ProcessState::Traced => 5,
            ProcessState::Idle => 6,
            ProcessState::Dead => 7,
            ProcessState::Unknown => 8,
        };
    }
    all
}

/// Every [`LinkState`], guarded by an exhaustive match.
fn link_states() -> Vec<LinkState> {
    let all = vec![
        LinkState::Up,
        LinkState::Down,
        LinkState::Dormant,
        LinkState::Unknown,
    ];
    for state in &all {
        let _guard: u8 = match state {
            LinkState::Up => 0,
            LinkState::Down => 1,
            LinkState::Dormant => 2,
            LinkState::Unknown => 3,
        };
    }
    all
}

/// Every [`ChargeState`], guarded by an exhaustive match.
fn charge_states() -> Vec<ChargeState> {
    let all = vec![
        ChargeState::Charging,
        ChargeState::Discharging,
        ChargeState::Full,
        ChargeState::NotCharging,
        ChargeState::Unknown,
    ];
    for state in &all {
        let _guard: u8 = match state {
            ChargeState::Charging => 0,
            ChargeState::Discharging => 1,
            ChargeState::Full => 2,
            ChargeState::NotCharging => 3,
            ChargeState::Unknown => 4,
        };
    }
    all
}

/// Every [`CapabilityState`], guarded by an exhaustive match.
fn capability_states() -> Vec<CapabilityState> {
    let all = vec![
        CapabilityState::Available,
        CapabilityState::Unsupported,
        CapabilityState::PermissionDenied,
        CapabilityState::Unknown,
    ];
    for state in &all {
        let _guard: u8 = match state {
            CapabilityState::Available => 0,
            CapabilityState::Unsupported => 1,
            CapabilityState::PermissionDenied => 2,
            CapabilityState::Unknown => 3,
        };
    }
    all
}

/// Every [`UnavailableReason`], guarded by an exhaustive match.
///
/// All ten matter: each one becomes the *text* of a cell that would otherwise show
/// a number, so each one is a string a `TERM=vt100` session has to be able to
/// print (§4, §5.1).
fn unavailable_reasons() -> Vec<UnavailableReason> {
    let all = vec![
        UnavailableReason::CounterReset,
        UnavailableReason::DeviceDisappeared,
        UnavailableReason::InterfaceRenamed,
        UnavailableReason::ProcessExited,
        UnavailableReason::ReadFailed,
        UnavailableReason::ParseFailed,
        UnavailableReason::Timeout,
        UnavailableReason::SkippedUnderLoad,
        UnavailableReason::LinkSpeedUnknown,
        UnavailableReason::NeedsSecondSample,
    ];
    for reason in &all {
        let _guard: u8 = match reason {
            UnavailableReason::CounterReset => 0,
            UnavailableReason::DeviceDisappeared => 1,
            UnavailableReason::InterfaceRenamed => 2,
            UnavailableReason::ProcessExited => 3,
            UnavailableReason::ReadFailed => 4,
            UnavailableReason::ParseFailed => 5,
            UnavailableReason::Timeout => 6,
            UnavailableReason::SkippedUnderLoad => 7,
            UnavailableReason::LinkSpeedUnknown => 8,
            UnavailableReason::NeedsSecondSample => 9,
        };
    }
    all
}

/// Every [`Severity`], guarded by an exhaustive match.
fn severities() -> Vec<Severity> {
    let all = vec![Severity::Info, Severity::Watch, Severity::Critical];
    for severity in &all {
        let _guard: u8 = match severity {
            Severity::Info => 0,
            Severity::Watch => 1,
            Severity::Critical => 2,
        };
    }
    all
}

/// Asserts `symbols` are pairwise distinct, naming `what` on failure.
fn assert_distinct(what: &str, symbols: &[char]) {
    let mut unique = symbols.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        symbols.len(),
        "{what} reuses a symbol: {symbols:?}"
    );
}

#[test]
fn every_state_enum_gives_each_variant_its_own_symbol() {
    // §5.2: colour may reinforce a state but must never be the only indicator, so
    // every state a colour distinguishes has to be distinguishable by character
    // alone *within its own type*. Across types the alphabets overlap on purpose —
    // `X` is critical for both `PressureState` and `Severity` — because a symbol is
    // only ever read inside one column.
    assert_distinct(
        "MetricState",
        &metric_states()
            .iter()
            .map(MetricState::symbol)
            .collect::<Vec<_>>(),
    );
    assert_distinct(
        "PressureState",
        &pressure_states()
            .iter()
            .map(|state| state.symbol())
            .collect::<Vec<_>>(),
    );
    // `ProcessState`'s cue is the familiar `ps` letter rather than a `symbol()`.
    assert_distinct(
        "ProcessState",
        &process_states()
            .iter()
            .map(|state| state.code())
            .collect::<Vec<_>>(),
    );
    assert_distinct(
        "LinkState",
        &link_states()
            .iter()
            .map(|state| state.symbol())
            .collect::<Vec<_>>(),
    );
    assert_distinct(
        "ChargeState",
        &charge_states()
            .iter()
            .map(|state| state.symbol())
            .collect::<Vec<_>>(),
    );
    assert_distinct(
        "CapabilityState",
        &capability_states()
            .iter()
            .map(|state| state.symbol())
            .collect::<Vec<_>>(),
    );
    assert_distinct(
        "Severity",
        &severities()
            .iter()
            .map(|severity| severity.symbol())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn every_state_enum_also_gives_each_variant_its_own_words() {
    // The symbol is one cell; where there is room the label is what a first-time
    // user actually reads, so it has to be distinct too.
    for group in [
        pressure_states()
            .iter()
            .map(|state| state.label())
            .collect::<Vec<_>>(),
        process_states()
            .iter()
            .map(|state| state.label())
            .collect::<Vec<_>>(),
        link_states()
            .iter()
            .map(|state| state.label())
            .collect::<Vec<_>>(),
        charge_states()
            .iter()
            .map(|state| state.label())
            .collect::<Vec<_>>(),
        capability_states()
            .iter()
            .map(|state| state.label())
            .collect::<Vec<_>>(),
        severities()
            .iter()
            .map(|severity| severity.label())
            .collect::<Vec<_>>(),
    ] {
        let mut unique = group.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), group.len(), "duplicate label in {group:?}");
        for label in group {
            assert!(!label.is_empty());
        }
    }
    // Every unavailable `MetricState` says *which* kind of unavailable it is (§4).
    let mut placeholders: Vec<&str> = metric_states()
        .iter()
        .filter_map(MetricState::placeholder)
        .collect();
    let placeholder_count = placeholders.len();
    placeholders.sort_unstable();
    placeholders.dedup();
    assert_eq!(placeholders.len(), placeholder_count);
    // All ten transient reasons share the `?` symbol, so the *words* are the only
    // thing that tells a counter reset from an interface rename. They must differ.
    let reasons: Vec<&str> = unavailable_reasons()
        .iter()
        .map(|reason| reason.message())
        .collect();
    let mut unique = reasons.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        reasons.len(),
        "two transient reasons read identically: {reasons:?}"
    );
}

#[test]
fn the_pressure_radar_puts_its_state_symbol_and_word_on_screen() {
    // §2.3 requires the raw metric, the severity, the rule, and an explicit
    // unavailable state. The symbol column is the cue that survives every depth.
    let normal = text_of(&frame(
        &reference(ViewId::Overview),
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::TrueColor),
    ));
    assert!(
        normal.contains(&format!("{} CPU", PressureState::Normal.symbol())),
        "a normal radar row lost its symbol:\n{normal}"
    );
    assert!(normal.contains("normal"), "{normal}");
    // §2.3's explicit unavailable state: the fake platform has no link speed, so
    // network pressure cannot be derived and must not read as `normal`.
    assert!(
        normal.contains("? NET"),
        "an undetermined signal did not read as unknown:\n{normal}"
    );

    let saturated = state_of(saturated_scenario(), 24, (140, 38), ViewId::Overview);
    let text = text_of(&frame(
        &saturated,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::TrueColor),
    ));
    assert!(
        text.contains(&format!("{} CPU", PressureState::Critical.symbol())),
        "a critical radar row lost its symbol:\n{text}"
    );
    assert!(text.contains("critical"), "{text}");
}

#[test]
fn the_process_table_puts_a_notable_state_code_in_the_marker_column() {
    // §7.2: zombie and uninterruptible-sleep rows must be visibly distinct, and the
    // marker column is priority 0 so the cue outlives the `STATE` column being
    // dropped in the Compact band (§5.7).
    for size in [(140, 38), (110, 30), (80, 24), (60, 16)] {
        let state = state_of(notable_scenario(), 24, size, ViewId::Processes);
        let text = text_of(&frame(
            &state,
            ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
        ));
        for notable in process_states().into_iter().filter(|s| s.is_notable()) {
            assert!(
                text.lines()
                    .any(|line| line.trim_start_matches('|').starts_with(notable.code())),
                "{notable:?} lost its marker at {size:?}:\n{text}"
            );
        }
    }
}

#[test]
fn the_network_screen_prefixes_a_link_state_with_its_own_symbol() {
    // §5.2: `up` is drawn as `+up`, so the state survives both `--color off` and a
    // monochrome screenshot.
    let state = reference(ViewId::Network);
    let text = text_of(&frame(
        &state,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
    ));
    let expected = format!("{}{}", LinkState::Up.symbol(), LinkState::Up.label());
    assert!(
        text.contains(&expected),
        "the link state lost its cue (wanted {expected:?}):\n{text}"
    );
}

#[test]
fn the_header_suffixes_the_battery_with_its_charge_state_symbol() {
    let state = reference(ViewId::Overview);
    let text = text_of(&frame(
        &state,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
    ));
    let expected = format!("82%{}", ChargeState::Discharging.symbol());
    assert!(
        text.contains(&expected),
        "the charge state lost its cue (wanted {expected:?}):\n{text}"
    );
}

#[test]
fn the_inspect_screen_prefixes_every_capability_row_with_its_state_symbol() {
    // §7.5 asks for the unavailable metrics *and why*. Each row leads with the
    // capability's own symbol so the panel is readable without colour.
    let state = state_of(
        Scenario::permission_denied(),
        24,
        (140, 38),
        ViewId::Inspect,
    );
    let text = text_of(&frame(
        &state,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
    ));
    assert!(text.contains("UNAVAILABLE METRICS"), "{text}");
    let mut seen = 0usize;
    for capability in capability_states() {
        if capability == CapabilityState::Available {
            continue;
        }
        let marker = format!("  {} ", capability.symbol());
        if text.contains(&marker) {
            seen += 1;
        }
    }
    assert!(
        seen > 0,
        "no capability row carried a state symbol:\n{text}"
    );
    // The reason is spelled out beside the symbol, never implied by colour alone.
    assert!(
        text.contains(CapabilityState::PermissionDenied.label())
            || text.contains(CapabilityState::Unsupported.label()),
        "{text}"
    );
}

#[test]
fn a_narrow_column_keeps_two_unavailable_states_apart_by_text() {
    // This began as a characterisation test for the weakest cue in the interface:
    // `permission denied` does not fit a narrow column, and degrading it straight to
    // `n/a` made it identical to `Unsupported`'s own placeholder, leaving only weight
    // — bold versus dim — to carry a distinction the interface exists to make.
    //
    // `states::abbreviated_placeholder` closed that gap by adding one rung to the
    // ladder, so the assertion is now the opposite one: down to six cells the two
    // states differ in *text*, which is the only cue that survives colour off, a
    // monochrome terminal, and a screen reader alike.
    let ascii_glyphs = GlyphSet::ascii();
    let denied: MetricState<u64> = MetricState::PermissionDenied;
    let unsupported: MetricState<u64> = MetricState::Unsupported;
    let denied = states::describe_display(&denied);
    let unsupported = states::describe_display(&unsupported);

    for width in 6..=16usize {
        assert_ne!(
            denied.fitted(width, ascii_glyphs),
            unsupported.fitted(width, ascii_glyphs),
            "width {width} collapses `permission denied` and `n/a` into one text"
        );
    }
    assert_eq!(denied.fitted(6, ascii_glyphs), "denied");
    assert_eq!(unsupported.fitted(6, ascii_glyphs), "n/a");

    // Below the abbreviation the text does collapse, and only the symbol separates
    // them. Three, four and five cells are the `CPU%`/`MEM%` columns, so this is a
    // real state of the interface rather than a hypothetical one — recorded here,
    // and in `docs/accessibility.md`, as what remains.
    for width in 3..=5usize {
        assert_eq!(
            denied.fitted(width, ascii_glyphs),
            unsupported.fitted(width, ascii_glyphs),
            "width {width} was expected to fall back to `n/a` for both"
        );
    }

    // What carries the distinction there: a different token, a different style even
    // with colour off, and a different one-cell symbol.
    assert_ne!(denied.token(), unsupported.token());
    let theme = ThemeId::HighContrast.theme();
    assert_ne!(
        theme.style(denied.token(), ColorDepth::Off),
        theme.style(unsupported.token(), ColorDepth::Off),
        "with colour off the two states would be indistinguishable"
    );
    assert_eq!(denied.fitted(17, ascii_glyphs), "permission denied");
    assert_ne!(
        denied.fitted(1, ascii_glyphs),
        unsupported.fitted(1, ascii_glyphs)
    );
}

#[test]
fn an_unavailable_metric_is_named_rather_than_zeroed_on_every_screen() {
    // §4's headline rule, checked through the renderer rather than through the type.
    for view in VIEWS {
        let state = state_of(Scenario::permission_denied(), 24, (140, 38), view);
        let text = text_of(&frame(
            &state,
            ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
        ));
        assert!(
            text.contains("permission denied") || text.contains("n/a"),
            "{view:?} rendered no placeholder at all:\n{text}"
        );
        assert!(
            !text.contains(" 0% ["),
            "{view:?} drew a zero bar for a refused metric:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. `--color off` stays legible (§5.2)
// ---------------------------------------------------------------------------

#[test]
fn every_colour_depth_renders_the_same_characters() {
    // The characters are the information; the colour only reinforces it. If a depth
    // changed a character, one of the two renderings would be carrying meaning the
    // other does not.
    for view in VIEWS {
        for theme in ThemeId::ALL.map(ThemeId::theme) {
            let state = state_of(notable_scenario(), 24, (140, 38), view);
            let baseline = text_of(&frame(&state, ascii(theme, ColorDepth::TrueColor)));
            for depth in ColorDepth::ALL {
                let text = text_of(&frame(&state, ascii(theme, depth)));
                assert_eq!(
                    text,
                    baseline,
                    "{view:?} in {} at {depth:?} changed a character",
                    theme.name()
                );
            }
        }
    }
}

#[test]
fn colour_off_still_distinguishes_selection_notable_states_and_pressure() {
    // The three things §5.2 makes non-negotiable, asserted on the frame a user with
    // `--color off` (or `NO_COLOR=1`, or a monochrome terminal) actually sees.
    let mut state = state_of(notable_scenario(), 24, (140, 38), ViewId::Processes);
    let _ = monitrs_tui::app::reduce(&mut state, monitrs_tui::action::Action::SelectNext);
    for theme in ThemeId::ALL.map(ThemeId::theme) {
        let buffer = frame(&state, ascii(theme, ColorDepth::Off));
        let text = text_of(&buffer);

        // Selection: the marker glyph, plus a reversed row. Reversing is the only
        // way to separate a row when both colours are `Reset`.
        let marker = GlyphSet::ascii().selection_marker();
        assert!(
            text.lines().any(|line| line.contains(marker)),
            "the selected row lost its marker in {}:\n{text}",
            theme.name()
        );
        let selection = theme.selection_style(ColorDepth::Off);
        assert!(selection.is_readable(), "{}", theme.name());
        assert!(selection.modifier.contains(Modifier::REVERSED));
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| cell.modifier.contains(Modifier::REVERSED)),
            "no cell was reversed, so no row reads as selected in {}",
            theme.name()
        );

        // Notable process states: the `ps` letter in the marker column.
        for notable in process_states().into_iter().filter(|s| s.is_notable()) {
            assert!(
                text.contains(notable.code()),
                "{notable:?} is invisible without colour in {}:\n{text}",
                theme.name()
            );
        }
    }

    // Pressure severity: the symbol *and* the word, at every severity the engine
    // can reach, with colour switched off.
    for (scenario, expected) in [
        (Scenario::default(), PressureState::Normal),
        (saturated_scenario(), PressureState::Critical),
    ] {
        let state = state_of(scenario, 24, (140, 38), ViewId::Overview);
        let text = text_of(&frame(
            &state,
            ascii(ThemeId::HighContrast.theme(), ColorDepth::Off),
        ));
        assert!(
            text.contains(&format!("{} CPU", expected.symbol())),
            "{expected:?} lost its symbol without colour:\n{text}"
        );
        assert!(text.contains(expected.label()), "{text}");
    }
}

#[test]
fn colour_off_keeps_the_three_pressure_states_apart_by_modifier() {
    // Belt and braces for the symbol: even the *style* still differs, because
    // `Token::emphasis` is depth-independent by construction.
    for theme in ThemeId::ALL.map(ThemeId::theme) {
        let good = theme.style(Token::Good, ColorDepth::Off);
        let watch = theme.style(Token::Watch, ColorDepth::Off);
        let critical = theme.style(Token::Critical, ColorDepth::Off);
        assert_ne!(good, watch, "{}", theme.name());
        assert_ne!(watch, critical, "{}", theme.name());
        assert_ne!(good, critical, "{}", theme.name());
        assert_eq!(good.fg, Some(Color::Reset), "{}", theme.name());
    }
}

// ---------------------------------------------------------------------------
// 3. Contrast (WCAG 2.1 relative luminance)
// ---------------------------------------------------------------------------

/// The six levels of the xterm 6×6×6 colour cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// xterm's default RGB for the sixteen named ANSI colours, in ANSI index order.
///
/// These sixteen values are **not** monitrs's to choose: every terminal ships its
/// own, and many users replace them. They are used here so that
/// [`ColorDepth::Ansi16`] gets a number at all, and `docs/accessibility.md` states
/// plainly that those columns are indicative rather than guaranteed.
const ANSI16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x00, 0x00),
    (0x00, 0xcd, 0x00),
    (0xcd, 0xcd, 0x00),
    (0x00, 0x00, 0xee),
    (0xcd, 0x00, 0xcd),
    (0x00, 0xcd, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x7f, 0x7f, 0x7f),
    (0xff, 0x00, 0x00),
    (0x00, 0xff, 0x00),
    (0xff, 0xff, 0x00),
    (0x5c, 0x5c, 0xff),
    (0xff, 0x00, 0xff),
    (0x00, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

/// The ANSI palette index of a named ratatui colour.
fn ansi_index(color: Color) -> Option<usize> {
    Some(match color {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        _ => return None,
    })
}

/// The RGB triple a colour resolves to, or `None` for [`Color::Reset`].
///
/// `Reset` means "whatever the terminal already uses", which has no measurable
/// luminance — which is exactly why [`ColorDepth::Off`] is absent from the contrast
/// tables and why the no-colour mode is checked by symbol instead.
fn rgb_of(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(index) => Some(indexed_rgb(index)),
        named => ansi_index(named).map(|index| ANSI16[index]),
    }
}

/// The xterm-256 palette: sixteen system colours, a 6×6×6 cube, 24 greys.
fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    let index = usize::from(index);
    if index < 16 {
        return ANSI16[index];
    }
    if index < 232 {
        let n = index - 16;
        return (
            CUBE_LEVELS[n / 36],
            CUBE_LEVELS[(n % 36) / 6],
            CUBE_LEVELS[n % 6],
        );
    }
    // 232..=255 are 8, 18, 28, ... 238.
    let level = 8 + 10 * u8::try_from(index - 232).unwrap_or(0);
    (level, level, level)
}

/// One channel, linearised per WCAG 2.1 SC 1.4.3.
fn linearise(channel: u8) -> f64 {
    let value = f64::from(channel) / 255.0;
    if value <= 0.039_28 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance.
fn luminance(rgb: (u8, u8, u8)) -> f64 {
    0.2126 * linearise(rgb.0) + 0.7152 * linearise(rgb.1) + 0.0722 * linearise(rgb.2)
}

/// The WCAG contrast ratio of two colours, or `None` when either is `Reset`.
fn contrast(theme: &Theme, depth: ColorDepth, fg: Token, bg: Token) -> Option<f64> {
    let front = luminance(rgb_of(theme.color(fg, depth))?);
    let back = luminance(rgb_of(theme.color(bg, depth))?);
    let (high, low) = if front >= back {
        (front, back)
    } else {
        (back, front)
    };
    Some((high + 0.05) / (low + 0.05))
}

/// The floor for text: WCAG 2.1 AA for body text.
const TEXT_FLOOR: f64 = 4.5;

/// The floor for a symbol, a bar, or a band: WCAG 2.1 AA for non-text contrast.
const SYMBOL_FLOOR: f64 = 3.0;

/// The pairs every theme must clear, and the floor each is held to.
///
/// The list is exactly the review's brief — text on `base`, text on `surface`, the
/// selected row, and each of `good`/`watch`/`critical` on their background — plus
/// `surface` for the three states, because `surface` is the background the overlays
/// paint and the signal-confirmation dialog is drawn in `critical`.
const REQUIRED: [(&str, Token, Token, f64); 9] = [
    ("text on base", Token::Text, Token::Base, TEXT_FLOOR),
    ("text on surface", Token::Text, Token::Surface, TEXT_FLOOR),
    ("selected row", Token::Text, Token::Selection, TEXT_FLOOR),
    ("good on base", Token::Good, Token::Base, TEXT_FLOOR),
    ("watch on base", Token::Watch, Token::Base, TEXT_FLOOR),
    ("critical on base", Token::Critical, Token::Base, TEXT_FLOOR),
    ("good on surface", Token::Good, Token::Surface, TEXT_FLOOR),
    ("watch on surface", Token::Watch, Token::Surface, TEXT_FLOOR),
    (
        "critical on surface",
        Token::Critical,
        Token::Surface,
        TEXT_FLOOR,
    ),
];

#[test]
fn every_theme_meets_the_contrast_floor_it_promises() {
    // The two failures this pins down were real when the review started:
    //
    // * `default-light` drew `good` in ANSI `Green` (2.16:1 on its own background)
    //   and `watch` in ANSI `Yellow` (1.70:1). The word `watch` in yellow on white
    //   is not readable, so the palette moved to the only ANSI names that clear
    //   4.5:1 on a light background.
    // * `default-light` at 256 colours drew `good` at 4.05:1 and `watch` at 4.06:1
    //   on `surface`, the background the overlays paint. Both indices went darker.
    for theme in ThemeId::ALL.map(ThemeId::theme) {
        for depth in ColorDepth::COLORED {
            for (label, fg, bg, floor) in REQUIRED {
                let ratio = contrast(theme, depth, fg, bg)
                    .expect("a coloured depth always resolves to an RGB triple");
                assert!(
                    ratio >= floor,
                    "{} at {depth:?}: {label} is {ratio:.2}:1, below {floor}:1",
                    theme.name()
                );
            }
        }
    }
}

#[test]
fn the_high_contrast_theme_is_actually_high_contrast() {
    // A theme that advertises maximum separation (§3.1) is held to the whole floor,
    // not to the four required pairs: every foreground token against `base`, and the
    // selected row's *band* against `base` as well as its text against the band.
    //
    // The band is what this pins down. `#0000c0` measured 1.76:1 against black —
    // blue contributes 7% of relative luminance, so a saturated dark blue band is
    // invisible on black however saturated it is.
    let theme = ThemeId::HighContrast.theme();
    for depth in ColorDepth::COLORED {
        for token in Token::ALL {
            if matches!(token, Token::Base | Token::Surface | Token::Selection) {
                continue;
            }
            let ratio = contrast(theme, depth, token, Token::Base).expect("resolvable");
            assert!(
                ratio >= TEXT_FLOOR,
                "high-contrast at {depth:?}: {} is {ratio:.2}:1 on base",
                token.name()
            );
        }
        let band = contrast(theme, depth, Token::Selection, Token::Base).expect("resolvable");
        assert!(
            band >= SYMBOL_FLOOR,
            "high-contrast at {depth:?}: the selection band is {band:.2}:1 against base"
        );
        let row = contrast(theme, depth, Token::Text, Token::Selection).expect("resolvable");
        assert!(
            row >= TEXT_FLOOR,
            "high-contrast at {depth:?}: the selected row is {row:.2}:1"
        );
    }
}

#[test]
fn the_documented_contrast_shortfalls_cannot_get_worse() {
    // These pairs are below their floor and are documented as such in
    // `docs/accessibility.md`, with the reason each was not changed. Pinning a floor
    // just under the measured value turns "known shortfall" into a regression test:
    // the numbers in the document stay true, and nobody can quietly make them worse.
    let shortfalls: [(ThemeId, ColorDepth, &str, Token, Token, f64); 11] = [
        // `stale` is DIM + ITALIC as well; see the document's note on DIM.
        (
            ThemeId::DefaultDark,
            ColorDepth::TrueColor,
            "stale on base",
            Token::Stale,
            Token::Base,
            3.8,
        ),
        (
            ThemeId::DefaultDark,
            ColorDepth::Ansi256,
            "stale on base",
            Token::Stale,
            Token::Base,
            3.7,
        ),
        (
            ThemeId::DefaultDark,
            ColorDepth::TrueColor,
            "border on base",
            Token::Border,
            Token::Base,
            1.8,
        ),
        (
            ThemeId::DefaultDark,
            ColorDepth::TrueColor,
            "selection band vs base",
            Token::Selection,
            Token::Base,
            1.4,
        ),
        (
            ThemeId::DefaultLight,
            ColorDepth::Ansi256,
            "muted on base",
            Token::Muted,
            Token::Base,
            3.9,
        ),
        (
            ThemeId::DefaultLight,
            ColorDepth::Ansi16,
            "muted on base",
            Token::Muted,
            Token::Base,
            4.0,
        ),
        (
            ThemeId::DefaultLight,
            ColorDepth::Ansi256,
            "stale on base",
            Token::Stale,
            Token::Base,
            3.4,
        ),
        (
            ThemeId::DefaultLight,
            ColorDepth::TrueColor,
            "border on base",
            Token::Border,
            Token::Base,
            1.6,
        ),
        (
            ThemeId::DefaultLight,
            ColorDepth::TrueColor,
            "selection band vs base",
            Token::Selection,
            Token::Base,
            1.3,
        ),
        (
            ThemeId::DefaultDark,
            ColorDepth::Ansi16,
            "accent on base",
            Token::Accent,
            Token::Base,
            4.4,
        ),
        (
            ThemeId::DefaultLight,
            ColorDepth::Ansi16,
            "graph_3 on base",
            Token::Graph3,
            Token::Base,
            1.7,
        ),
    ];
    for (id, depth, label, fg, bg, floor) in shortfalls {
        let ratio = contrast(id.theme(), depth, fg, bg).expect("resolvable");
        assert!(
            ratio >= floor,
            "{} at {depth:?}: {label} fell to {ratio:.2}:1, below its documented {floor}:1",
            id.name()
        );
    }
}

#[test]
fn contrast_report_for_every_theme_and_depth() {
    // The table `docs/accessibility.md` publishes. Regenerate it with:
    //
    //   cargo test -p monitrs-tui --test accessibility -- --nocapture contrast_report
    //
    // The assertions are the two above; this exists so the published numbers come
    // from the same code that enforces them and cannot drift apart.
    let mut pairs: Vec<(String, Token, Token, f64)> = REQUIRED
        .iter()
        .map(|(label, fg, bg, floor)| ((*label).to_owned(), *fg, *bg, *floor))
        .collect();
    for token in [
        Token::Accent,
        Token::Muted,
        Token::Stale,
        Token::FocusBorder,
        Token::Border,
    ] {
        let floor = if matches!(token, Token::Border | Token::FocusBorder) {
            SYMBOL_FLOOR
        } else {
            TEXT_FLOOR
        };
        pairs.push((
            format!("{} on base", token.name()),
            token,
            Token::Base,
            floor,
        ));
    }
    pairs.push((
        "selection band vs base".to_owned(),
        Token::Selection,
        Token::Base,
        SYMBOL_FLOOR,
    ));
    for token in Token::GRAPHS {
        pairs.push((
            format!("{} on base", token.name()),
            token,
            Token::Base,
            SYMBOL_FLOOR,
        ));
    }

    for id in ThemeId::ALL {
        println!("\n#### `{}`\n", id.name());
        println!("| pair | min | truecolor | 256 | 16 |");
        println!("|---|---:|---:|---:|---:|");
        for (label, fg, bg, floor) in &pairs {
            let mut cells = String::new();
            for depth in [
                ColorDepth::TrueColor,
                ColorDepth::Ansi256,
                ColorDepth::Ansi16,
            ] {
                let ratio = contrast(id.theme(), depth, *fg, *bg).expect("resolvable");
                let mark = if ratio >= *floor { "" } else { " **!**" };
                cells.push_str(&format!(" {ratio:.2}{mark} |"));
            }
            println!("| `{label}` | {floor} |{cells}");
        }
        // `Off` has no measurable luminance; it is covered by the symbol tests.
        assert_eq!(id.theme().color(Token::Text, ColorDepth::Off), Color::Reset);
    }
}

// ---------------------------------------------------------------------------
// 4. Strict ASCII is genuinely 7-bit (§5.1)
// ---------------------------------------------------------------------------

/// Asserts every byte of `text` is printable 7-bit ASCII.
fn assert_seven_bit(what: &str, text: &str) {
    for byte in text.bytes() {
        assert!(
            (0x20..=0x7e).contains(&byte),
            "{what} = {text:?} contains byte {byte:#04x}"
        );
    }
}

#[test]
fn strict_ascii_frames_are_seven_bit_on_every_screen_at_every_size() {
    // `glyphs.rs` proves the *inventory* is ASCII. This proves the *output* is: five
    // screens, ten fixtures, and five sizes, including the sizes where the layout
    // switches to the minimal list and to the resize notice.
    for view in VIEWS {
        for (name, scenario, samples) in fixtures() {
            for size in SIZES {
                let state = state_of(scenario.clone(), samples, size, view);
                let buffer = frame(
                    &state,
                    ascii(ThemeId::DefaultDark.theme(), ColorDepth::TrueColor),
                );
                for cell in buffer.content() {
                    assert_seven_bit(&format!("{view:?}/{name}/{size:?}"), cell.symbol());
                }
            }
        }
    }
}

#[test]
fn every_state_string_the_model_can_produce_is_seven_bit() {
    // The frame test above only covers the states a fixture reaches. These strings
    // are the complete set the model can hand the renderer, so a future variant
    // spelled with a typographic dash is caught here rather than on a user's
    // `TERM=vt100` session.
    for reason in unavailable_reasons() {
        assert_seven_bit("UnavailableReason::message", reason.message());
        assert_seven_bit("UnavailableReason::Display", &reason.to_string());
        let state: MetricState<u64> = MetricState::TemporarilyUnavailable(reason);
        assert_seven_bit(
            "MetricState::placeholder",
            state.placeholder().unwrap_or_default(),
        );
    }
    for state in metric_states() {
        assert_seven_bit("MetricState::symbol", &state.symbol().to_string());
        assert_seven_bit(
            "MetricState::placeholder",
            state.placeholder().unwrap_or_default(),
        );
    }
    for state in pressure_states() {
        assert_seven_bit("PressureState::symbol", &state.symbol().to_string());
        assert_seven_bit("PressureState::label", state.label());
    }
    for state in process_states() {
        assert_seven_bit("ProcessState::code", &state.code().to_string());
        assert_seven_bit("ProcessState::label", state.label());
    }
    for state in link_states() {
        assert_seven_bit("LinkState::symbol", &state.symbol().to_string());
        assert_seven_bit("LinkState::label", state.label());
    }
    for state in charge_states() {
        assert_seven_bit("ChargeState::symbol", &state.symbol().to_string());
        assert_seven_bit("ChargeState::label", state.label());
    }
    for state in capability_states() {
        assert_seven_bit("CapabilityState::symbol", &state.symbol().to_string());
        assert_seven_bit("CapabilityState::label", state.label());
    }
    for severity in severities() {
        assert_seven_bit("Severity::symbol", &severity.symbol().to_string());
        assert_seven_bit("Severity::label", severity.label());
    }
}

#[test]
fn every_chrome_string_the_interface_owns_is_seven_bit() {
    // Column headers, breakpoint names, theme names, and the generated help are all
    // written by hand, so they are exactly where a stray `–` or `…` would appear.
    for column in Column::DISPLAY_ORDER {
        assert_seven_bit("Column::header", column.header());
    }
    for breakpoint in [
        Breakpoint::TooSmall,
        Breakpoint::Compact,
        Breakpoint::Standard,
        Breakpoint::Wide,
    ] {
        assert_seven_bit("Breakpoint::label", breakpoint.label());
    }
    for id in ThemeId::ALL {
        assert_seven_bit("ThemeId::name", id.name());
    }
    for token in Token::ALL {
        assert_seven_bit("Token::name", token.name());
    }
    for glyph in Glyph::ALL {
        assert_seven_bit("GlyphSet::ascii", GlyphSet::ascii().get(glyph));
    }
    let keymap = Keymap::builtin();
    for binding in keymap.bindings() {
        assert_seven_bit("Chord::label", &binding.chord.label());
        assert_seven_bit("Binding::description", binding.description);
    }
    for mode in InputMode::ALL {
        for section in keymap.help(mode) {
            assert_seven_bit("HelpSection::title", section.title);
            for entry in section.entries {
                assert_seven_bit("HelpEntry::keys", &entry.keys);
                assert_seven_bit("HelpEntry::description", entry.description);
            }
        }
    }
}

#[test]
fn enhanced_mode_is_the_only_mode_that_emits_non_ascii() {
    // The other half of the promise: if the enhanced set had silently regressed to
    // ASCII, `--glyphs ascii` would be trivially satisfied and prove nothing.
    let state = reference(ViewId::Overview);
    let unicode = frame(
        &state,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::TrueColor).with_glyphs(GlyphSet::unicode()),
    );
    assert!(
        unicode
            .content()
            .iter()
            .any(|cell| !cell.symbol().is_ascii()),
        "enhanced mode produced a frame with no non-ASCII character at all"
    );
}

// ---------------------------------------------------------------------------
// 5. No flashing, no animation (§3.2, §5.2)
// ---------------------------------------------------------------------------

#[test]
fn no_frame_of_any_screen_carries_a_blink_attribute() {
    // §5.2 forbids continuously alternating colours and §3.2 forbids animated
    // effects that reduce legibility. Blink is both, and it is the one terminal
    // attribute that cannot be turned off by the reader.
    for view in VIEWS {
        for (name, scenario, samples) in fixtures() {
            let state = state_of(scenario.clone(), samples, (140, 38), view);
            for depth in ColorDepth::ALL {
                let buffer = frame(&state, ascii(ThemeId::HighContrast.theme(), depth));
                for cell in buffer.content() {
                    assert!(
                        !cell.modifier.contains(Modifier::SLOW_BLINK),
                        "{view:?}/{name} at {depth:?} emitted SLOW_BLINK"
                    );
                    assert!(
                        !cell.modifier.contains(Modifier::RAPID_BLINK),
                        "{view:?}/{name} at {depth:?} emitted RAPID_BLINK"
                    );
                }
            }
        }
    }
}

#[test]
fn the_same_state_renders_identically_however_often_it_is_drawn() {
    // Nothing in a render reads a clock (§10.5): the frame is a pure function of the
    // state. That is what makes "no animation" a property rather than a promise —
    // there is no time input for an animation to be a function of.
    for view in VIEWS {
        let state = state_of(saturated_scenario(), 24, (140, 38), view);
        let presentation = ascii(ThemeId::DefaultDark.theme(), ColorDepth::TrueColor);
        let first = frame(&state, presentation);
        for _ in 0..3 {
            assert_eq!(
                frame(&state, presentation),
                first,
                "{view:?} did not render identically twice"
            );
        }
    }
}

#[test]
fn two_states_built_at_different_wall_times_render_identically() {
    // Stronger than repeating one render: two independently built states, whose
    // `Instant::now()` and construction order differ, must produce the same frame.
    // A style that keyed off elapsed time would differ here.
    let first = state_of(Scenario::default(), 24, (140, 38), ViewId::Overview);
    let second = state_of(Scenario::default(), 24, (140, 38), ViewId::Overview);
    let presentation = ascii(ThemeId::DefaultDark.theme(), ColorDepth::TrueColor);
    assert_eq!(
        text_of(&frame(&first, presentation)),
        text_of(&frame(&second, presentation))
    );
}

#[test]
fn no_action_is_reachable_only_through_a_timed_key_sequence() {
    // The only timing requirement in the interface is the 500 ms window for a
    // two-key sequence, and `KeyResolver`'s timeout is not configurable from the
    // command line or the configuration file. That is only acceptable while every
    // sequence has a single-key alternative in the same mode, which is what this
    // asserts: a user who cannot press two keys inside half a second loses nothing.
    let keymap = Keymap::builtin();
    for binding in keymap.bindings() {
        if !binding.chord.is_sequence() {
            continue;
        }
        let alternative = keymap.bindings().iter().any(|other| {
            !other.chord.is_sequence()
                && other.outcome == binding.outcome
                && other.modes.iter().any(|mode| binding.modes.contains(mode))
        });
        assert!(
            alternative,
            "{} is the only way to reach {}, and it is timed",
            binding.chord.label(),
            binding.outcome.diagnostic_name()
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Narrow terminals (§5.7)
// ---------------------------------------------------------------------------

#[test]
fn the_eighty_by_twenty_four_layout_keeps_identity_and_the_headline_numbers() {
    // 80×24 is the size every terminal is guaranteed to have, so it is the size the
    // review treats as the real baseline rather than the wide dashboard.
    let state = state_of(notable_scenario(), 24, (80, 24), ViewId::Overview);
    let text = text_of(&frame(
        &state,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
    ));
    assert_eq!(Breakpoint::resolve(80, 24), Breakpoint::Compact);
    for header in ["PID", "USER", "CPU%", "MEM%", "RSS", "NAME"] {
        assert!(
            text.contains(header),
            "{header} was dropped at 80x24:\n{text}"
        );
    }
    // §5.7's Compact band: the one-line summary and the tab strip survive.
    assert!(text.contains("CPU"), "{text}");
    assert!(text.contains("1 Overview"), "{text}");
    assert!(text.contains("? help"), "{text}");
    // Dropped, and named in the document as dropped.
    for dropped in ["VIRT", "THR", "PRESSURE", "HISTORY", "PINS"] {
        assert!(
            !text.contains(dropped),
            "{dropped} is still drawn at 80x24, so the document is wrong:\n{text}"
        );
    }
    for line in text.lines() {
        assert_eq!(display_width(line), 80, "a row is not 80 cells: {line:?}");
    }
}

#[test]
fn the_sixty_by_sixteen_layout_still_identifies_and_ranks_processes() {
    // §5.7: "render a stable minimal process list if at least 60x16". The review's
    // question is whether that list is still *usable*: can you tell which process
    // is which, and which one is selected.
    let state = state_of(notable_scenario(), 24, (60, 16), ViewId::Overview);
    let text = text_of(&frame(
        &state,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
    ));
    assert_eq!(Breakpoint::resolve(60, 16), Breakpoint::TooSmall);
    for header in ["PID", "USER", "CPU%", "MEM%", "RSS", "NAME"] {
        assert!(
            text.contains(header),
            "{header} was dropped at 60x16:\n{text}"
        );
    }
    assert!(
        text.contains(GlyphSet::ascii().selection_marker()),
        "the selection marker was dropped at 60x16:\n{text}"
    );
    for notable in process_states().into_iter().filter(|s| s.is_notable()) {
        assert!(
            text.contains(notable.code()),
            "{notable:?} lost its cue at 60x16:\n{text}"
        );
    }
    // Dropped at this step, and named in the document as dropped.
    for dropped in ["READ/s", "WRITE/s", "1 Overview", "? help"] {
        assert!(
            !text.contains(dropped),
            "{dropped} is still drawn at 60x16, so the document is wrong:\n{text}"
        );
    }
    for line in text.lines() {
        assert_eq!(display_width(line), 60, "a row is not 60 cells: {line:?}");
    }
}

#[test]
fn below_sixty_by_sixteen_the_notice_is_plain_readable_text() {
    // The one screen that has to work when nothing else fits. §5.7 fixes the three
    // lines; the accessibility point is that they are prose, not a diagram, and that
    // they name the current size so the user knows how far to drag.
    let state = state_of(Scenario::default(), 24, (52, 12), ViewId::Overview);
    let text = text_of(&frame(
        &state,
        ascii(ThemeId::DefaultDark.theme(), ColorDepth::Off),
    ));
    assert!(text.contains("monitrs needs at least 60x16"), "{text}");
    assert!(text.contains("current terminal: 52x12"), "{text}");
    assert!(text.contains("resize or press q to quit"), "{text}");
    for line in text.lines() {
        assert_seven_bit("resize notice", line);
    }
}

#[test]
fn a_zero_area_terminal_renders_nothing_and_panics_at_nothing() {
    // §5.7's hard rule, at the boundary the review cares about: a user dragging a
    // split to nothing must not take the process down.
    for size in [(0, 0), (1, 1), (0, 24), (80, 0), (2, 2)] {
        let state = state_of(Scenario::default(), 3, size, ViewId::Overview);
        for depth in ColorDepth::ALL {
            let _ = frame(&state, ascii(ThemeId::HighContrast.theme(), depth));
        }
    }
}
