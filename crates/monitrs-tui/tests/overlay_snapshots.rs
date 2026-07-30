//! Snapshot tests for the overlay layer (§17.3).
//!
//! Every fixture comes from `monitrs-collectors`' [`FakeCollector`] or from the app
//! state itself, for the reason §17.5 gives: a deterministic fake can be put into
//! states a real machine cannot be asked for on demand — a zombie process, a refused
//! per-process I/O read, a configuration that had to be clamped — and hand-written
//! structs would let a test drift away from what the rest of monitrs can actually
//! produce.
//!
//! Each overlay is snapshotted in **both glyph modes**, because §5.1's two modes are
//! first-class: the ASCII snapshot is the one that proves strict mode emits nothing but
//! printable 7-bit characters, and the Unicode snapshot is the one that proves the
//! layout did not move when the characters changed.
//!
//! The confirmation dialog gets four snapshots of its own — `SIGTERM`, `SIGKILL`, the
//! signal menu, and a zombie — because it is the one overlay where a rendering mistake
//! is a safety problem rather than a cosmetic one (§15.1).
//!
//! Nothing here reads a clock. Wall-clock times come from the fake's fixed
//! `SystemTime::UNIX_EPOCH` tick and from an explicit start time, so the snapshots are
//! stable (§17.3's "normalize nondeterministic timestamps and hostnames").

// An integration test is its own crate, so the library's `cfg(test)` allowance does not
// reach here. `expect` is how a test asserts a precondition: a fixture that cannot be
// built is a broken test, and failing loudly at that line is the wanted behaviour.
// Production code in this crate keeps both lints denied (§18.2).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use core::time::Duration;
use std::time::{Instant, SystemTime};

use monitrs_collectors::fake::{FakeCollector, FakeProcess, Pattern, Scenario};
use monitrs_collectors::source::{SampleTick, SnapshotSource};
use monitrs_collectors::tier::DueTiers;
use monitrs_core::SystemSnapshot;
use monitrs_core::history::{HistoricalSample, HistoryConfig, HistoryRing};
use monitrs_core::model::{
    ProcessDetail, ProcessDetailResult, ProcessIdentity, ProcessSnapshot, ProcessState,
};
use monitrs_core::process::{ProcessSort, ProcessSortKey, SortDirection};
use monitrs_tui::action::{PendingProcessAction, SignalKind};
use monitrs_tui::app::{AppSettings, AppState, Notice, NoticeKind, ProcessActionStage, TextInput};
use monitrs_tui::glyphs::GlyphSet;
use monitrs_tui::keymap::{InputMode, Keymap};
use monitrs_tui::theme::{ColorDepth, ThemeId};
use monitrs_tui::views::overlays::{
    CommandPaletteOverlay, FilterEditOverlay, HelpOverlay, NoticeOverlay, ProcessActionOverlay,
    ProcessDetailOverlay, SortSelectorOverlay, SpikeAttributionOverlay,
};
use monitrs_tui::widgets::Presentation;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A fixed monotonic origin, so nothing in a fixture depends on when it ran.
fn origin() -> Instant {
    Instant::now()
}

/// A fixed wall-clock origin: 2026-07-29T22:13:30Z, thirty-seven seconds before the
/// moment §5.6's mockup is written around.
fn wall_origin() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_363_210)
}

/// Samples `scenario` `count` times, one second apart, and returns every snapshot.
fn samples(scenario: Scenario, count: u64) -> Vec<SystemSnapshot> {
    let mut collector = FakeCollector::new(scenario);
    let start = origin();
    let mut tick = SampleTick::first(start, wall_origin());
    let mut collected = Vec::new();
    for index in 0..count {
        if index > 0 {
            tick = tick.advance(
                start + Duration::from_secs(index),
                wall_origin() + Duration::from_secs(index),
                DueTiers::ALL,
            );
        }
        if let Ok(snapshot) = collector.sample(&tick) {
            collected.push(snapshot);
        }
    }
    collected
}

/// The `sequence`-th snapshot of `scenario`.
fn sample_at(scenario: Scenario, sequence: u64) -> SystemSnapshot {
    samples(scenario, sequence + 1)
        .into_iter()
        .next_back()
        .expect("the fake collector produced no snapshot")
}

/// The reference process: `rustc` at its spike, from §5.5's own scenario.
fn spiking_process() -> ProcessSnapshot {
    let snapshot = sample_at(Scenario::default(), 20);
    snapshot
        .processes
        .iter()
        .find(|process| &*process.name == "rustc")
        .cloned()
        .expect("the reference scenario has a rustc")
}

/// A process the kernel has already reaped, which §15.1 forbids pretending to signal.
fn reaped_process() -> ProcessSnapshot {
    let scenario = Scenario {
        processes: vec![
            FakeProcess::new(31_842, 900_100, "rustc", "cargo build --release")
                .with_state(ProcessState::Zombie)
                .with_cpu(Pattern::Steady(0.0)),
        ],
        ..Scenario::default()
    };
    sample_at(scenario, 3)
        .processes
        .first()
        .cloned()
        .expect("the zombie scenario has one process")
}

/// The on-demand detail of the reference process (§8.6).
fn reference_detail(identity: ProcessIdentity) -> ProcessDetail {
    let mut collector = FakeCollector::new(Scenario::default());
    match collector.process_detail(identity) {
        ProcessDetailResult::Loaded(detail) => *detail,
        other => panic!("the fake collector refused a detail it should have: {other:?}"),
    }
}

/// A retained history sample at its CPU spike, with the §2.2 contributor evidence.
fn spike_sample() -> HistoricalSample {
    let mut ring = HistoryRing::with_config(HistoryConfig::default(), origin());
    for snapshot in samples(Scenario::default(), 21) {
        let _ = ring.record(&snapshot);
    }
    ring.newest()
        .cloned()
        .expect("twenty-one samples were recorded")
}

/// The notices a session with a bad configuration and a refusing collector produces.
///
/// The clamp warning comes from [`AppState`] itself rather than being written out, so
/// the snapshot shows the message §8.5 actually generates.
fn session_notices() -> AppState {
    let mut state = AppState::new(AppSettings {
        started_at: origin(),
        size: (100, 30),
        history: HistoryConfig {
            duration: Duration::from_secs(60 * 60 * 24),
            ..HistoryConfig::default()
        },
        ..AppSettings::default()
    });
    state.push_notice(Notice::watch(
        NoticeKind::Collector,
        "/proc/diskstats read failed",
    ));
    state.push_notice(Notice::watch(
        NoticeKind::Permission,
        "per-process I/O is not permitted at this privilege level; monitrs does not escalate",
    ));
    state.push_notice(Notice::info(
        NoticeKind::Export,
        "wrote 12 KiB to /tmp/monitrs.json",
    ));
    state.push_notice(Notice::critical(
        NoticeKind::ProcessAction,
        "PID 31842 has already exited; no signal was sent",
    ));
    state
}

// ---------------------------------------------------------------------------
// Rendering harness
// ---------------------------------------------------------------------------

/// Renders `draw` into a `TestBackend` and returns the buffer.
fn render<F: FnOnce(&mut Buffer, Rect)>(width: u16, height: u16, draw: F) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height))
        .expect("a test backend never fails to initialise");
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw(frame.buffer_mut(), area);
        })
        .expect("drawing to a test backend never fails");
    terminal.backend().buffer().clone()
}

/// The characters of a buffer, one framed line per row.
///
/// The frame makes trailing whitespace visible: an overlay that stops one cell short of
/// its border is a §5.4 bug that an unframed snapshot would hide. A double-width
/// grapheme's continuation cell is skipped rather than printed, so a Japanese process
/// name does not appear as `\u{65e5} \u{672c}` and read as a bug it is not.
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

/// Asserts that every character in the buffer is printable 7-bit ASCII (§5.1).
fn assert_strict_ascii(buffer: &Buffer) {
    for cell in buffer.content() {
        for byte in cell.symbol().bytes() {
            assert!(
                (0x20..=0x7e).contains(&byte),
                "strict ASCII mode emitted {:?}",
                cell.symbol()
            );
        }
    }
}

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

/// The width and height every overlay snapshot is taken at.
///
/// Comfortably larger than the overlays so the snapshots show the whole panel and its
/// placement inside the frame; the 80×24 degradation has its own test.
const WIDTH: u16 = 104;

/// Rows for the taller overlays.
const HEIGHT: u16 = 30;

// ---------------------------------------------------------------------------
// §7.6: help
// ---------------------------------------------------------------------------

#[test]
fn help_overlay_ascii() {
    let sections = Keymap::builtin().help(InputMode::Normal);
    let buffer = render(WIDTH, 40, |buffer, area| {
        HelpOverlay::new(ascii(), &sections, InputMode::Normal).render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn help_overlay_unicode() {
    let sections = Keymap::builtin().help(InputMode::Normal);
    let buffer = render(WIDTH, 40, |buffer, area| {
        HelpOverlay::new(unicode(), &sections, InputMode::Normal).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn help_overlay_is_context_aware() {
    // §7.6: the same overlay in a different mode lists a different keymap.
    let sections = Keymap::builtin().help(InputMode::ConfirmProcessAction);
    let buffer = render(WIDTH, 20, |buffer, area| {
        HelpOverlay::new(ascii(), &sections, InputMode::ConfirmProcessAction).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

// ---------------------------------------------------------------------------
// §15.1: the confirmation dialog
// ---------------------------------------------------------------------------

/// The stage that confirms `signal` against the reference process.
fn confirming(signal: SignalKind, identity: ProcessIdentity) -> ProcessActionStage {
    ProcessActionStage::Confirm(PendingProcessAction::Signal { identity, signal })
}

#[test]
fn signal_confirmation_sigterm_ascii() {
    let process = spiking_process();
    let stage = confirming(SignalKind::Term, process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(ascii(), stage, Some(&process)).render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    let text = text_of(&buffer);
    assert!(text.contains("SIGTERM"), "{text}");
    assert!(text.contains("Enter"), "§6.2: the confirmation key\n{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn signal_confirmation_sigterm_unicode() {
    let process = spiking_process();
    let stage = confirming(SignalKind::Term, process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(unicode(), stage, Some(&process)).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn signal_confirmation_sigkill_ascii() {
    let process = spiking_process();
    let stage = confirming(SignalKind::Kill, process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(ascii(), stage, Some(&process)).render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    let text = text_of(&buffer);
    // §15.1: the forceful confirmation is distinct from ordinary Enter, and the dialog
    // has to say so rather than leaving the user to discover it.
    assert!(text.contains("Enter will not confirm"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn signal_confirmation_sigkill_unicode() {
    let process = spiking_process();
    let stage = confirming(SignalKind::Kill, process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(unicode(), stage, Some(&process)).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn signal_confirmation_for_a_zombie_ascii() {
    // §15.1: an already-exited process is clearly reported, and no confirmation is
    // offered for something that cannot happen.
    let process = reaped_process();
    let stage = confirming(SignalKind::Term, process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(ascii(), stage, Some(&process)).render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    let text = text_of(&buffer);
    assert!(text.contains("already exited"), "{text}");
    assert!(!text.contains("Enter send"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn signal_confirmation_for_a_zombie_unicode() {
    let process = reaped_process();
    let stage = confirming(SignalKind::Term, process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(unicode(), stage, Some(&process)).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn signal_menu_ascii() {
    // §9.2: SIGKILL last and marked forceful.
    let process = spiking_process();
    let stage = ProcessActionStage::ChooseSignal {
        identity: process.identity,
        cursor: 0,
    };
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(ascii(), stage, Some(&process)).render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn signal_menu_unicode() {
    let process = spiking_process();
    let stage = ProcessActionStage::ChooseSignal {
        identity: process.identity,
        cursor: 3,
    };
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessActionOverlay::new(unicode(), stage, Some(&process)).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn signal_confirmation_at_eighty_by_twenty_four() {
    // §5.7's compact band. The confirmation key must still be on screen.
    let process = spiking_process();
    let stage = confirming(SignalKind::Kill, process.identity);
    let buffer = render(80, 24, |buffer, area| {
        ProcessActionOverlay::new(ascii(), stage, Some(&process)).render(area, buffer);
    });
    let text = text_of(&buffer);
    assert!(text.contains('Y'), "{text}");
    insta::assert_snapshot!(text);
}

// ---------------------------------------------------------------------------
// §2.2, §5.6: spike attribution
// ---------------------------------------------------------------------------

#[test]
fn spike_attribution_ascii() {
    let sample = spike_sample();
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        SpikeAttributionOverlay::for_sample(ascii(), &sample)
            .with_offset("-00:37")
            .render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    let text = text_of(&buffer);
    assert!(text.contains("top contributors"), "{text}");
    assert!(text.contains("evidence coverage"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn spike_attribution_unicode() {
    let sample = spike_sample();
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        SpikeAttributionOverlay::for_sample(unicode(), &sample)
            .with_offset("-00:37")
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn spike_attribution_with_refused_disk_readings() {
    // §2.2: a platform that withheld the per-process readings must not be given a
    // flattering coverage figure.
    let mut ring = HistoryRing::with_config(HistoryConfig::default(), origin());
    for snapshot in samples(Scenario::permission_denied(), 6) {
        let _ = ring.record(&snapshot);
    }
    let sample = ring.newest().cloned().expect("samples were recorded");
    let buffer = render(WIDTH, 20, |buffer, area| {
        SpikeAttributionOverlay::for_sample(ascii(), &sample)
            .with_offset("-00:02")
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

// ---------------------------------------------------------------------------
// §2.4, §7.5: process detail
// ---------------------------------------------------------------------------

#[test]
fn process_detail_ascii() {
    let process = spiking_process();
    let detail = reference_detail(process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessDetailOverlay::new(ascii(), process.identity, Some(&process), Some(&detail))
            .render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn process_detail_unicode() {
    let process = spiking_process();
    let detail = reference_detail(process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessDetailOverlay::new(unicode(), process.identity, Some(&process), Some(&detail))
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn process_detail_before_the_on_demand_read_arrives() {
    // §8.6 loads this asynchronously, so the first frame has no detail and must not
    // read as "this process has no working directory".
    let process = spiking_process();
    let buffer = render(WIDTH, 20, |buffer, area| {
        ProcessDetailOverlay::new(ascii(), process.identity, Some(&process), None)
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn process_detail_with_refused_metrics() {
    let snapshot = sample_at(Scenario::permission_denied(), 4);
    let process = snapshot
        .processes
        .first()
        .cloned()
        .expect("the scenario has processes");
    let detail = reference_detail(process.identity);
    let buffer = render(WIDTH, HEIGHT, |buffer, area| {
        ProcessDetailOverlay::new(ascii(), process.identity, Some(&process), Some(&detail))
            .render(area, buffer);
    });
    let text = text_of(&buffer);
    assert!(
        text.contains("permission denied") || text.contains("n/a"),
        "{text}"
    );
    insta::assert_snapshot!(text);
}

// ---------------------------------------------------------------------------
// §6.2: the filter editor and the sort selector
// ---------------------------------------------------------------------------

#[test]
fn filter_edit_ascii() {
    let input = TextInput::seeded("rustc");
    let buffer = render(WIDTH, 12, |buffer, area| {
        FilterEditOverlay::new(ascii(), &input, 1, 5).render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn filter_edit_unicode() {
    let input = TextInput::seeded("rustc");
    let buffer = render(WIDTH, 12, |buffer, area| {
        FilterEditOverlay::new(unicode(), &input, 1, 5).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn filter_edit_with_no_match() {
    let input = TextInput::seeded("does-not-exist");
    let buffer = render(WIDTH, 8, |buffer, area| {
        FilterEditOverlay::new(ascii(), &input, 0, 5).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn sort_selector_ascii() {
    let buffer = render(WIDTH, 20, |buffer, area| {
        SortSelectorOverlay::new(ascii(), ProcessSort::descending(ProcessSortKey::Cpu), 0)
            .render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn sort_selector_unicode() {
    let buffer = render(WIDTH, 20, |buffer, area| {
        SortSelectorOverlay::new(
            unicode(),
            ProcessSort::new(ProcessSortKey::Memory, SortDirection::Ascending),
            3,
        )
        .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

// ---------------------------------------------------------------------------
// §6.3: the command palette
// ---------------------------------------------------------------------------

#[test]
fn command_palette_ascii() {
    let input = TextInput::new();
    let buffer = render(WIDTH, 20, |buffer, area| {
        CommandPaletteOverlay::new(ascii(), &input, 0).render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn command_palette_unicode() {
    let input = TextInput::new();
    let buffer = render(WIDTH, 20, |buffer, area| {
        CommandPaletteOverlay::new(unicode(), &input, 2).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn command_palette_narrowed_by_what_was_typed() {
    let input = TextInput::seeded("export snapshot /tmp/monitrs.json");
    let buffer = render(WIDTH, 8, |buffer, area| {
        CommandPaletteOverlay::new(ascii(), &input, 0).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

// ---------------------------------------------------------------------------
// §14.1, §21 M6: errors and notices
// ---------------------------------------------------------------------------

#[test]
fn notice_overlay_ascii() {
    let state = session_notices();
    let buffer = render(WIDTH, 16, |buffer, area| {
        NoticeOverlay::new(ascii(), state.notices())
            .with_dropped(state.notice_log().dropped())
            .with_dismiss_hint("Esc")
            .render(area, buffer);
    });
    assert_strict_ascii(&buffer);
    let text = text_of(&buffer);
    // §8.5, §21 M6: the clamped configuration must be reported, not silently applied.
    assert!(text.contains("clamped"), "{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn notice_overlay_unicode() {
    let state = session_notices();
    let buffer = render(WIDTH, 16, |buffer, area| {
        NoticeOverlay::new(unicode(), state.notices())
            .with_dropped(state.notice_log().dropped())
            .with_dismiss_hint("Esc")
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn notice_overlay_at_eighty_by_twenty_four() {
    let state = session_notices();
    let buffer = render(80, 24, |buffer, area| {
        NoticeOverlay::new(ascii(), state.notices())
            .with_dismiss_hint("Esc")
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

// ---------------------------------------------------------------------------
// Cross-cutting properties
// ---------------------------------------------------------------------------

/// The first and last non-blank column of each row.
fn occupied_columns(buffer: &Buffer) -> Vec<(u16, Option<(u16, u16)>)> {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| {
            let filled: Vec<u16> = (buffer.area.left()..buffer.area.right())
                .filter(|x| {
                    buffer
                        .cell((*x, y))
                        .is_some_and(|cell| cell.symbol() != " ")
                })
                .collect();
            let span = match (filled.first(), filled.last()) {
                (Some(first), Some(last)) => Some((*first, *last)),
                _ => None,
            };
            (y, span)
        })
        .collect()
}

#[test]
fn the_two_glyph_modes_occupy_the_same_columns() {
    // §5.1: switching `--glyphs` changes the characters, never the layout. Cell-for-cell
    // equality is deliberately not the claim — the marker and the border differ — but
    // every row must start and end in the same column.
    let process = spiking_process();
    let detail = reference_detail(process.identity);
    let stage = confirming(SignalKind::Kill, process.identity);
    let sample = spike_sample();

    for (name, plain, rich) in [
        (
            "confirmation",
            render(WIDTH, HEIGHT, |buffer, area| {
                ProcessActionOverlay::new(ascii(), stage, Some(&process)).render(area, buffer);
            }),
            render(WIDTH, HEIGHT, |buffer, area| {
                ProcessActionOverlay::new(unicode(), stage, Some(&process)).render(area, buffer);
            }),
        ),
        (
            "detail",
            render(WIDTH, HEIGHT, |buffer, area| {
                ProcessDetailOverlay::new(ascii(), process.identity, Some(&process), Some(&detail))
                    .render(area, buffer);
            }),
            render(WIDTH, HEIGHT, |buffer, area| {
                ProcessDetailOverlay::new(
                    unicode(),
                    process.identity,
                    Some(&process),
                    Some(&detail),
                )
                .render(area, buffer);
            }),
        ),
        (
            "attribution",
            render(WIDTH, HEIGHT, |buffer, area| {
                SpikeAttributionOverlay::for_sample(ascii(), &sample).render(area, buffer);
            }),
            render(WIDTH, HEIGHT, |buffer, area| {
                SpikeAttributionOverlay::for_sample(unicode(), &sample).render(area, buffer);
            }),
        ),
    ] {
        assert_eq!(
            occupied_columns(&plain),
            occupied_columns(&rich),
            "{name} moved when the glyph mode changed"
        );
    }
}

#[test]
fn no_overlay_writes_outside_the_frame_at_any_size() {
    // §5.7: never panic because a calculated rectangle has zero width or height, and
    // never write outside it. Every overlay, at every awkward size.
    let process = spiking_process();
    let detail = reference_detail(process.identity);
    let sample = spike_sample();
    let sections = Keymap::builtin().help(InputMode::Normal);
    let state = session_notices();
    let input = TextInput::seeded("rustc");

    for (width, height) in [
        (0u16, 0u16),
        (0, 24),
        (80, 0),
        (1, 1),
        (2, 3),
        (60, 16),
        (80, 24),
        (104, 30),
    ] {
        for presentation in [ascii(), unicode()] {
            let buffer = render(width.max(1), height.max(1), |buffer, area| {
                let area = Rect {
                    width,
                    height,
                    ..area
                };
                HelpOverlay::new(presentation, &sections, InputMode::Normal).render(area, buffer);
                ProcessActionOverlay::new(
                    presentation,
                    confirming(SignalKind::Kill, process.identity),
                    Some(&process),
                )
                .render(area, buffer);
                ProcessDetailOverlay::new(
                    presentation,
                    process.identity,
                    Some(&process),
                    Some(&detail),
                )
                .render(area, buffer);
                SpikeAttributionOverlay::for_sample(presentation, &sample).render(area, buffer);
                FilterEditOverlay::new(presentation, &input, 1, 5).render(area, buffer);
                SortSelectorOverlay::new(
                    presentation,
                    ProcessSort::descending(ProcessSortKey::Cpu),
                    0,
                )
                .render(area, buffer);
                CommandPaletteOverlay::new(presentation, &input, 0).render(area, buffer);
                NoticeOverlay::new(presentation, state.notices())
                    .with_dismiss_hint("Esc")
                    .render(area, buffer);
            });
            assert_eq!(
                buffer.area,
                Rect::new(0, 0, width.max(1), height.max(1)),
                "the buffer was resized at {width}x{height}"
            );
        }
    }
}
