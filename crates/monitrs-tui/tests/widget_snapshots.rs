//! Snapshot tests for the reusable widgets (§17.3).
//!
//! Every fixture comes from `monitrs-collectors`' [`FakeCollector`], because §17.3
//! asks for snapshots of states a real machine cannot be put into on demand — the
//! warming-up first frame, permission-denied metrics, stale data, an empty process
//! list — and §17.5 requires those to come from a deterministic fake rather than
//! from hand-written structs. Hand-building a `SystemSnapshot` here would also let
//! a test drift away from what a collector can actually produce.
//!
//! Two things are snapshotted, not one:
//!
//! * the **characters**, through ratatui's `TestBackend`, which is what a user
//!   reads; and
//! * the **styles**, as a run-length map, because a `TestBackend` view discards
//!   them entirely — and a no-colour snapshot that looks identical to a true-colour
//!   one would prove nothing about §5.2's "colour is never the only indicator".
//!
//! Nondeterminism is excluded rather than normalised: the fake host name is fixed,
//! its uptime is a function of the sample sequence, and no widget in this crate
//! reads a clock (§17.3's "normalize nondeterministic timestamps and hostnames").

// An integration test is its own crate, so the library's `cfg(test)` allowance does
// not reach here. `expect` is how a test asserts a precondition: a fixture that
// cannot be built is a broken test, and failing loudly at that line is exactly the
// behaviour wanted. Production code in this crate keeps both lints denied (§18.2).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use core::fmt::Write as _;
use core::time::Duration;
use std::time::{Instant, SystemTime};

use monitrs_collectors::fake::{FakeCollector, FakeProcess, Pattern, Scenario};
use monitrs_collectors::source::{SampleTick, SnapshotSource};
use monitrs_collectors::tier::DueTiers;
use monitrs_core::SystemSnapshot;
use monitrs_core::model::{
    MeasuredValue, Measurement, MetricState, PressureId, PressureSignal, PressureState,
    ProcessIdentity, ProcessSnapshot,
};
use monitrs_core::units::{ByteUnits, Percent};
use monitrs_tui::glyphs::GlyphSet;
use monitrs_tui::layout::TableLayout;
use monitrs_tui::theme::{ColorDepth, ThemeId, Token};
use monitrs_tui::widgets::{
    CoreStrip, Meter, PinRow, Pins, Presentation, ProcessRow, ProcessTable, Radar, Sparkline,
    SparklineCaret,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Borders, Widget};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Samples `scenario` `count` times and returns every snapshot.
///
/// The tick advances by exactly one second per sample, which is presentation only:
/// the fake's rate arithmetic uses `tick.elapsed` and never assumes an interval
/// (§8.1).
fn samples(scenario: Scenario, count: u64) -> Vec<SystemSnapshot> {
    let mut collector = FakeCollector::new(scenario);
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
    let mut collected = Vec::new();
    for index in 0..count {
        if index > 0 {
            tick = tick.advance(
                start + Duration::from_secs(index),
                SystemTime::UNIX_EPOCH,
                DueTiers::ALL,
            );
        }
        if let Ok(snapshot) = collector.sample(&tick) {
            collected.push(snapshot);
        }
    }
    collected
}

/// The `count`-th snapshot of `scenario`, counting from sequence zero.
fn sample_at(scenario: Scenario, sequence: u64) -> SystemSnapshot {
    let collected = samples(scenario, sequence + 1);
    collected
        .into_iter()
        .next_back()
        .expect("the fake collector produced no snapshot")
}

/// The system-wide CPU history of `scenario`, as a sparkline series.
fn cpu_history(scenario: Scenario, count: u64) -> Vec<MetricState<Percent>> {
    samples(scenario, count)
        .iter()
        .map(|snapshot| snapshot.cpu.total.map(|usage| usage.busy))
        .collect()
}

/// Per-core utilizations as a [`CoreStrip`] series.
///
/// An unavailable per-core read becomes one unavailable entry per logical CPU
/// rather than an empty list: §4 forbids collapsing "not measured" into "no cores".
fn per_core(snapshot: &SystemSnapshot) -> Vec<MetricState<Percent>> {
    match snapshot.cpu.per_core.fresh() {
        Some(cores) => cores
            .iter()
            .map(|core| MetricState::Available(core.busy))
            .collect(),
        None => (0..snapshot.cpu.logical_count)
            .map(|_| snapshot.cpu.per_core.as_ref().map(|_| Percent::ZERO))
            .collect(),
    }
}

/// The §5.5 radar, derived from a snapshot the way the diagnostic engine will.
///
/// The rules are the ones §11.2 states, spelled out here because §2.3 requires the
/// rule text to travel with the signal.
fn radar_signals(snapshot: &SystemSnapshot) -> Vec<PressureSignal> {
    let cpu_busy = snapshot.cpu.total.map(|usage| usage.busy);
    let network = snapshot
        .networks
        .first()
        .map_or(MetricState::Unsupported, |interface| {
            interface.utilization()
        });
    vec![
        PressureSignal {
            id: PressureId::Cpu,
            state: cpu_busy.map(|busy| {
                if busy.value() >= 85.0 {
                    PressureState::Critical
                } else if busy.value() >= 60.0 {
                    PressureState::Watch
                } else {
                    PressureState::Normal
                }
            }),
            severity: cpu_busy,
            raw: cpu_busy
                .fresh()
                .map(|busy| Measurement::new("busy", MeasuredValue::Percent(*busy))),
            rule: "busy >= 85% sustained for 10 of 15 samples",
            held_for: Some(Duration::from_secs(12)),
        },
        PressureSignal {
            id: PressureId::Memory,
            state: snapshot.memory.usage.map(|used| {
                if used.value() >= 90.0 {
                    PressureState::Critical
                } else if used.value() >= 70.0 {
                    PressureState::Watch
                } else {
                    PressureState::Normal
                }
            }),
            severity: snapshot.memory.usage,
            raw: snapshot
                .memory
                .available
                .fresh()
                .map(|bytes| Measurement::new("available", MeasuredValue::Bytes(*bytes))),
            rule: "available < 15% of total for 10 of 15 samples",
            held_for: None,
        },
        PressureSignal {
            id: PressureId::Network,
            state: network.map(|_| PressureState::Normal),
            severity: network,
            raw: snapshot
                .networks
                .first()
                .and_then(|interface| interface.rx.fresh().copied())
                .map(|rate| Measurement::new("down", MeasuredValue::ByteRate(rate))),
            rule: "utilization requires a known link speed",
            held_for: None,
        },
        PressureSignal::unsupported(PressureId::PsiIo, "Linux PSI is not available here"),
    ]
}

/// A scenario whose processes sit exactly on the byte-unit boundaries §5.4 warns
/// about, so a snapshot shows whether the RSS column reflows across them.
fn boundary_scenario() -> Scenario {
    let boundaries: [(&str, u64); 7] = [
        ("just-under-kib", 1_023),
        ("exactly-kib", 1_024),
        ("just-under-mib", 1_048_575),
        ("exactly-mib", 1_048_576),
        ("just-under-gib", 1_073_741_823),
        ("exactly-gib", 1_073_741_824),
        ("largest-possible", u64::MAX),
    ];
    Scenario {
        processes: boundaries
            .iter()
            .enumerate()
            .map(|(index, (name, bytes))| {
                let pid = 100 + u32::try_from(index).unwrap_or(0);
                FakeProcess::new(pid, u64::from(pid) * 3, name, name)
                    .with_cpu(Pattern::Steady(1.0))
                    .with_rss(*bytes)
            })
            .collect(),
        ..Scenario::default()
    }
}

/// A scenario with the double-width process name §17.3 asks for.
fn unicode_name_scenario() -> Scenario {
    Scenario {
        processes: vec![
            FakeProcess::new(
                4_242,
                900_500,
                "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30d7}\u{30ed}\u{30bb}\u{30b9}\u{540d}",
                "/usr/local/bin/\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30d7}\u{30ed}\u{30bb}\u{30b9}\u{540d} --\u{5f15}\u{6570}",
            )
            .with_cpu(Pattern::Steady(42.0))
            .with_rss(512 * 1024 * 1024),
            FakeProcess::new(4_243, 900_501, "ascii-sibling", "ascii-sibling --flag")
                .with_cpu(Pattern::Steady(7.5))
                .with_rss(64 * 1024 * 1024),
        ],
        ..Scenario::default()
    }
}

// ---------------------------------------------------------------------------
// Rendering harness
// ---------------------------------------------------------------------------

/// Renders `draw` into a `TestBackend` of `width` x `height` and returns the buffer.
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
/// The frame makes trailing whitespace visible, which matters: a widget that stops
/// one cell short of its column is a §5.4 bug and an unframed snapshot hides it.
///
/// A double-width grapheme occupies two cells and ratatui blanks the second, so the
/// continuation cell is skipped rather than printed as a space — otherwise a
/// Japanese process name would appear in the snapshot as `\u{65e5} \u{672c} \u{8a9e}`
/// and read as a rendering bug it is not.
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
///
/// `TestBackend`'s own view discards styling, so this is what makes a no-colour
/// snapshot differ from a coloured one. Colours are printed with `Debug` because a
/// theme's exact palette is not the point — that two states differ, and keep
/// differing with colour switched off, is.
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

/// The four presentations §17.3 names: strict ASCII, enhanced Unicode, no colour.
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

/// Draws the widget gallery: one of every widget, composed the way §5.5 composes
/// them, into a single frame.
///
/// This is a *fixture*, not a screen. It exists so one snapshot covers the whole
/// module and so the widgets are exercised side by side, where a one-cell
/// misalignment between the radar and the history block is obvious.
fn gallery(
    buffer: &mut Buffer,
    area: Rect,
    presentation: Presentation<'_>,
    snapshot: &SystemSnapshot,
    history: &[MetricState<Percent>],
) {
    use monitrs_tui::widgets::Panel;

    let width = area.width;
    let signals = radar_signals(snapshot);
    let processes: Vec<ProcessSnapshot> = snapshot.processes.clone();
    let selected = processes.get(1).map(|process| process.identity);
    let rows: Vec<ProcessRow<'_>> = processes
        .iter()
        .enumerate()
        .map(|(index, process)| {
            ProcessRow::new(process)
                .selected(selected == Some(process.identity))
                .pinned(index == 0)
        })
        .collect();
    let pins: Vec<PinRow<'_>> = processes
        .iter()
        .take(2)
        .map(|process| PinRow::from_process(process, Some(MetricState::Available(Percent::FULL))))
        .collect();

    let row = |y: u16, height: u16| Rect::new(area.x, area.y + y, width, height);
    let inset =
        |y: u16, height: u16| Rect::new(area.x + 1, area.y + y, width.saturating_sub(2), height);

    // Header panel with the meters inside it.
    Panel::new(presentation, "monitrs host:dev-mbp  LIVE  1.0s")
        .with_trailing("up 3d 04:12")
        .focused(true)
        .with_borders(Borders::ALL.difference(Borders::BOTTOM))
        .render(row(0, 3), buffer);
    Meter::new(presentation, snapshot.cpu.total.map(|usage| usage.busy))
        .with_label("CPU")
        .with_label_width(4)
        .with_note("load 4.12 3.84 3.21")
        .render(inset(1, 1), buffer);
    Meter::new(presentation, snapshot.memory.usage)
        .with_label("MEM")
        .with_label_width(4)
        .with_note("swap 0.2G")
        .render(inset(2, 1), buffer);

    // Pressure radar beside the history block.
    let split = width / 2;
    Panel::new(presentation, "PRESSURE")
        .with_borders(Borders::ALL.difference(Borders::BOTTOM))
        .render(Rect::new(area.x, area.y + 3, split, 6), buffer);
    Radar::new(presentation, &signals).with_bars(false).render(
        Rect::new(area.x + 1, area.y + 4, split.saturating_sub(2), 4),
        buffer,
    );
    Panel::new(presentation, "HISTORY 5m")
        .with_borders(Borders::ALL.difference(Borders::BOTTOM))
        .render(
            Rect::new(area.x + split, area.y + 3, width - split, 6),
            buffer,
        );
    let plot = Rect::new(
        area.x + split + 1,
        area.y + 4,
        width.saturating_sub(split + 2),
        1,
    );
    Sparkline::new(presentation, history)
        .with_label("CPU")
        .with_label_width(4)
        .with_token(Token::Graph1)
        .render(plot, buffer);
    Sparkline::new(presentation, history)
        .with_label("I/O")
        .with_label_width(4)
        .dense(true)
        .with_token(Token::Graph3)
        .render(
            Rect {
                y: plot.y + 1,
                ..plot
            },
            buffer,
        );
    CoreStrip::new(presentation, &per_core(snapshot))
        .with_label("CORE")
        .with_label_width(4)
        .with_count(false)
        .render(
            Rect {
                y: plot.y + 2,
                ..plot
            },
            buffer,
        );
    SparklineCaret::new(presentation, history, 6)
        .with_label("CPU")
        .with_label_width(4)
        .with_note_segments(&["-00:06 selected".to_owned()])
        .render(
            Rect {
                y: plot.y + 3,
                ..plot
            },
            buffer,
        );

    // Process table.
    Panel::new(presentation, "PROCESSES")
        .with_trailing(&format!("{} total", snapshot.process_count()))
        .with_borders(Borders::ALL.difference(Borders::BOTTOM))
        .render(row(9, 9), buffer);
    let table_area = Rect::new(area.x + 1, area.y + 10, width.saturating_sub(2), 7);
    let layout = TableLayout::for_area(table_area);
    ProcessTable::new(presentation, &layout, &rows).render(table_area, buffer);

    // Pins.
    Panel::new(presentation, "PINS")
        .with_trailing("vs 30s")
        .render(row(17, 5), buffer);
    Pins::new(presentation, &pins).render(
        Rect::new(area.x + 1, area.y + 18, width.saturating_sub(2), 2),
        buffer,
    );
}

/// The three-row process table used by the focused snapshots.
fn table_only(
    buffer: &mut Buffer,
    area: Rect,
    presentation: Presentation<'_>,
    processes: &[ProcessSnapshot],
    selected: Option<ProcessIdentity>,
) {
    let rows: Vec<ProcessRow<'_>> = processes
        .iter()
        .map(|process| ProcessRow::new(process).selected(selected == Some(process.identity)))
        .collect();
    let layout = TableLayout::for_area(area);
    ProcessTable::new(presentation, &layout, &rows).render(area, buffer);
}

// ---------------------------------------------------------------------------
// §17.3: glyph modes
// ---------------------------------------------------------------------------

#[test]
fn ascii_mode_gallery() {
    let snapshot = sample_at(Scenario::default(), 20);
    let history = cpu_history(Scenario::default(), 40);
    let buffer = render(100, 22, |buffer, area| {
        gallery(buffer, area, ascii(), &snapshot, &history);
    });
    // §5.1: strict mode is provably 7-bit for everything the design system emits.
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
fn unicode_mode_gallery() {
    let snapshot = sample_at(Scenario::default(), 20);
    let history = cpu_history(Scenario::default(), 40);
    let buffer = render(100, 22, |buffer, area| {
        gallery(buffer, area, unicode(), &snapshot, &history);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

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
    // A user switching `--glyphs` must not see the layout move; only the characters
    // change (§5.1). Cell-for-cell equality is deliberately *not* the claim: enhanced
    // mode packs two samples into one Braille cell, so a dense history row legitimately
    // has a different number of filled cells at its left edge. What must hold is that
    // every row starts and ends in the same column.
    let snapshot = sample_at(Scenario::default(), 20);
    let history = cpu_history(Scenario::default(), 40);
    let plain = render(100, 22, |buffer, area| {
        gallery(buffer, area, ascii(), &snapshot, &history);
    });
    let rich = render(100, 22, |buffer, area| {
        gallery(buffer, area, unicode(), &snapshot, &history);
    });
    assert_eq!(occupied_columns(&plain), occupied_columns(&rich));
}

// ---------------------------------------------------------------------------
// §17.3: colour modes
// ---------------------------------------------------------------------------

#[test]
fn no_color_mode_gallery() {
    let snapshot = sample_at(Scenario::default(), 20);
    let history = cpu_history(Scenario::default(), 40);
    let buffer = render(100, 22, |buffer, area| {
        gallery(buffer, area, no_color(), &snapshot, &history);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn no_color_mode_styles() {
    // The characters are identical with colour on and off, so this is the snapshot
    // that shows meaning survives: §5.2's states must still differ by modifier.
    let snapshot = sample_at(Scenario::permission_denied(), 3);
    let buffer = render(80, 4, |buffer, area| {
        table_only(
            buffer,
            area,
            no_color(),
            &snapshot.processes,
            snapshot.processes.first().map(|process| process.identity),
        );
    });
    insta::assert_snapshot!(styles_of(&buffer));
}

#[test]
fn true_color_mode_styles() {
    let snapshot = sample_at(Scenario::permission_denied(), 3);
    let buffer = render(80, 4, |buffer, area| {
        table_only(
            buffer,
            area,
            ascii(),
            &snapshot.processes,
            snapshot.processes.first().map(|process| process.identity),
        );
    });
    insta::assert_snapshot!(styles_of(&buffer));
}

#[test]
fn colour_never_carries_information_on_its_own() {
    // The same frame with and without colour must contain the same characters.
    // If turning colour off lost information, this is where it would show.
    let snapshot = sample_at(Scenario::permission_denied(), 3);
    let coloured = render(80, 4, |buffer, area| {
        table_only(buffer, area, ascii(), &snapshot.processes, None);
    });
    let plain = render(80, 4, |buffer, area| {
        table_only(buffer, area, no_color(), &snapshot.processes, None);
    });
    assert_eq!(text_of(&coloured), text_of(&plain));
    assert_ne!(
        styles_of(&coloured),
        styles_of(&plain),
        "the two colour depths must be distinguishable at all"
    );
}

// ---------------------------------------------------------------------------
// §17.3: metric states
// ---------------------------------------------------------------------------

#[test]
fn empty_process_list() {
    // A locked-down container really can show nothing, and it is not a failure.
    let snapshot = sample_at(Scenario::empty(), 3);
    assert_eq!(snapshot.process_count(), 0);
    let buffer = render(90, 6, |buffer, area| {
        table_only(buffer, area, ascii(), &snapshot.processes, None);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn permission_denied_metrics() {
    let snapshot = sample_at(Scenario::permission_denied(), 3);
    assert!(snapshot.capabilities.any_permission_denied());
    let buffer = render(90, 10, |buffer, area| {
        let presentation = ascii();
        Meter::new(presentation, snapshot.cpu.total.map(|usage| usage.busy))
            .with_label("CPU")
            .with_label_width(4)
            .render(Rect { height: 1, ..area }, buffer);
        Meter::new(presentation, snapshot.memory.usage)
            .with_label("MEM")
            .with_label_width(4)
            .render(
                Rect {
                    y: area.y + 1,
                    height: 1,
                    ..area
                },
                buffer,
            );
        Radar::new(presentation, &radar_signals(&snapshot)).render(
            Rect {
                y: area.y + 2,
                height: 4,
                ..area
            },
            buffer,
        );
        table_only(
            buffer,
            Rect {
                y: area.y + 6,
                height: 4,
                ..area
            },
            presentation,
            &snapshot.processes,
            None,
        );
    });
    let text = text_of(&buffer);
    assert!(
        text.contains("permission denied") || text.contains("n/a"),
        "{text}"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn stale_metrics_are_marked_and_carry_their_age() {
    // §4: a retained value may be shown only if it is visibly marked stale *and*
    // carries its age.
    let scenario = Scenario {
        stale_from: Some(3),
        ..Scenario::default()
    };
    let snapshot = sample_at(scenario.clone(), 8);
    assert!(snapshot.cpu.total.is_stale());
    let history = cpu_history(scenario, 40);
    let buffer = render(90, 6, |buffer, area| {
        let presentation = ascii();
        Meter::new(presentation, snapshot.cpu.total.map(|usage| usage.busy))
            .with_label("CPU")
            .with_label_width(4)
            .render(Rect { height: 1, ..area }, buffer);
        Meter::new(presentation, snapshot.memory.usage)
            .with_label("MEM")
            .with_label_width(4)
            .render(
                Rect {
                    y: area.y + 1,
                    height: 1,
                    ..area
                },
                buffer,
            );
        Sparkline::new(presentation, &history)
            .with_label("CPU")
            .with_label_width(4)
            .render(
                Rect {
                    y: area.y + 2,
                    height: 1,
                    ..area
                },
                buffer,
            );
        table_only(
            buffer,
            Rect {
                y: area.y + 3,
                height: 3,
                ..area
            },
            presentation,
            &snapshot.processes,
            None,
        );
    });
    let text = text_of(&buffer);
    assert!(text.contains('~'), "the stale marker is missing:\n{text}");
    insta::assert_snapshot!(text);
}

#[test]
fn warming_up_first_frame() {
    // §8.2, §26: the first delta sample is warming up, never zero. This snapshot is
    // the one that would change if a widget ever rendered `0%` here.
    let snapshot = sample_at(Scenario::default(), 0);
    assert!(snapshot.cpu.total.is_warming_up());
    let history = cpu_history(Scenario::default(), 1);
    let buffer = render(90, 8, |buffer, area| {
        let presentation = ascii();
        Meter::new(presentation, snapshot.cpu.total.map(|usage| usage.busy))
            .with_label("CPU")
            .with_label_width(4)
            .render(Rect { height: 1, ..area }, buffer);
        Meter::new(presentation, snapshot.memory.usage)
            .with_label("MEM")
            .with_label_width(4)
            .render(
                Rect {
                    y: area.y + 1,
                    height: 1,
                    ..area
                },
                buffer,
            );
        Sparkline::new(presentation, &history)
            .with_label("CPU")
            .with_label_width(4)
            .render(
                Rect {
                    y: area.y + 2,
                    height: 1,
                    ..area
                },
                buffer,
            );
        Radar::new(presentation, &radar_signals(&snapshot)).render(
            Rect {
                y: area.y + 3,
                height: 2,
                ..area
            },
            buffer,
        );
        table_only(
            buffer,
            Rect {
                y: area.y + 5,
                height: 3,
                ..area
            },
            presentation,
            &snapshot.processes,
            None,
        );
    });
    let text = text_of(&buffer);
    assert!(
        !text.contains("0B/s"),
        "a warming-up rate rendered as zero:\n{text}"
    );
    assert!(
        !text.contains(" 0%"),
        "a warming-up percentage rendered as zero:\n{text}"
    );
    insta::assert_snapshot!(text);
}

// ---------------------------------------------------------------------------
// §17.3: values, names, and core counts
// ---------------------------------------------------------------------------

#[test]
fn values_crossing_the_byte_unit_boundaries() {
    // §5.4: `1023B -> 1.0KiB` must not reflow the table. Every row below is one
    // cell either side of a boundary.
    let snapshot = sample_at(boundary_scenario(), 3);
    let buffer = render(100, 9, |buffer, area| {
        table_only(buffer, area, ascii(), &snapshot.processes, None);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn values_crossing_the_byte_unit_boundaries_in_si_units() {
    let snapshot = sample_at(boundary_scenario(), 3);
    let buffer = render(100, 9, |buffer, area| {
        table_only(
            buffer,
            area,
            ascii().with_units(ByteUnits::Si),
            &snapshot.processes,
            None,
        );
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn a_long_unicode_process_name() {
    let snapshot = sample_at(unicode_name_scenario(), 3);
    let buffer = render(100, 4, |buffer, area| {
        table_only(buffer, area, unicode(), &snapshot.processes, None);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn a_long_unicode_process_name_in_a_narrow_table() {
    // The interesting case: a name of double-width characters truncated inside a
    // column narrow enough that a `char`-counting implementation would overflow it.
    let snapshot = sample_at(unicode_name_scenario(), 3);
    let buffer = render(62, 4, |buffer, area| {
        table_only(buffer, area, unicode(), &snapshot.processes, None);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn a_high_core_count() {
    // §7.1: hundreds of cores must be aggregated, not rendered as hundreds of rows.
    let snapshot = sample_at(Scenario::many_cores(), 3);
    assert_eq!(snapshot.cpu.logical_count, 256);
    let cores = per_core(&snapshot);
    let buffer = render(100, 5, |buffer, area| {
        let presentation = ascii();
        for (index, width) in [100u16, 80, 40, 20, 8].into_iter().enumerate() {
            let Ok(y) = u16::try_from(index) else { break };
            CoreStrip::new(presentation, &cores)
                .with_label("CORES")
                .with_count(true)
                .render(Rect::new(area.x, area.y + y, width, 1), buffer);
        }
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn a_high_core_count_in_enhanced_mode() {
    let snapshot = sample_at(Scenario::many_cores(), 3);
    let cores = per_core(&snapshot);
    let buffer = render(100, 2, |buffer, area| {
        CoreStrip::new(unicode(), &cores)
            .with_label("CORES")
            .with_count(true)
            .render(Rect { height: 1, ..area }, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

// ---------------------------------------------------------------------------
// §17.3: individual widgets worth pinning on their own
// ---------------------------------------------------------------------------

#[test]
fn the_pressure_radar_with_its_rules() {
    // §2.3: the rule that derived each state must be visible.
    let snapshot = sample_at(Scenario::default(), 20);
    let signals = radar_signals(&snapshot);
    let buffer = render(76, 8, |buffer, area| {
        Radar::new(ascii(), &signals)
            .with_rules(true)
            .with_bars(false)
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn the_pressure_radar_with_severity_bars() {
    let snapshot = sample_at(Scenario::default(), 20);
    let signals = radar_signals(&snapshot);
    let buffer = render(60, 4, |buffer, area| {
        Radar::new(ascii(), &signals)
            .with_bars(true)
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn the_pinned_process_strip_with_comparison_deltas() {
    // §2.5: pins with a stated baseline, a rising delta, a falling one, and a pin
    // whose process has gone.
    let earlier = sample_at(Scenario::default(), 5);
    let later = sample_at(Scenario::default(), 20);
    let baseline = |identity: ProcessIdentity| {
        earlier
            .process(identity)
            .map_or(MetricState::WarmingUp, |process| process.cpu)
    };
    let pins: Vec<PinRow<'_>> = later
        .processes
        .iter()
        .take(3)
        .map(|process| PinRow::from_process(process, Some(baseline(process.identity))))
        .chain(core::iter::once(PinRow::exited(
            "gone",
            ProcessIdentity::new(7_777, 1),
        )))
        .collect();
    let buffer = render(70, 4, |buffer, area| {
        Pins::new(ascii(), &pins)
            .with_baseline_label("vs 15s")
            .render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn the_process_table_in_tree_mode() {
    use monitrs_core::process::{ProcessSort, ProcessSortKey, ProcessTree};
    use monitrs_tui::widgets::tree_prefixes;

    let scenario = Scenario {
        processes: vec![
            FakeProcess::new(1, 1, "launchd", "/sbin/launchd").with_cpu(Pattern::Steady(0.1)),
            FakeProcess::new(500, 2, "sshd", "sshd: gabor").with_cpu(Pattern::Steady(1.0)),
            FakeProcess::new(501, 3, "bash", "-bash").with_cpu(Pattern::Steady(2.0)),
            FakeProcess::new(502, 4, "cargo", "cargo build").with_cpu(Pattern::Steady(9.0)),
            FakeProcess::new(503, 5, "rustc", "rustc --crate-name monitrs")
                .with_cpu(Pattern::Steady(287.0)),
            FakeProcess::new(600, 6, "cron", "/usr/sbin/cron").with_cpu(Pattern::Steady(0.0)),
        ],
        ..Scenario::default()
    };
    let mut snapshot = sample_at(scenario, 3);
    // Give the fixture a real shape: sshd -> bash -> cargo -> rustc under launchd.
    for (pid, parent) in [(500u32, 1u32), (501, 500), (502, 501), (503, 502), (600, 1)] {
        if let Some(process) = snapshot
            .processes
            .iter_mut()
            .find(|process| process.identity.pid == pid)
        {
            process.parent_pid = Some(parent);
        }
    }
    let tree = ProcessTree::build(
        &snapshot.processes,
        ProcessSort::ascending(ProcessSortKey::Pid),
    );
    let prefixes = tree_prefixes(GlyphSet::unicode(), &tree, 24);
    let rows: Vec<ProcessRow<'_>> = tree
        .rows()
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let process = snapshot.processes.get(row.process_index)?;
            let prefix = prefixes.get(index)?;
            Some(
                ProcessRow::new(process)
                    .with_prefix(prefix)
                    .selected(row.depth == 3),
            )
        })
        .collect();
    let buffer = render(100, 8, |buffer, area| {
        let layout = TableLayout::for_area(area);
        ProcessTable::new(unicode(), &layout, &rows).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn the_process_table_in_ascii_tree_mode() {
    use monitrs_core::process::{ProcessSort, ProcessSortKey, ProcessTree};
    use monitrs_tui::widgets::tree_prefixes;

    let mut snapshot = sample_at(Scenario::default(), 3);
    for (pid, parent) in [(31_842u32, 1u32), (1_221, 1), (507, 1), (9_182, 507)] {
        if let Some(process) = snapshot
            .processes
            .iter_mut()
            .find(|process| process.identity.pid == pid)
        {
            process.parent_pid = Some(parent);
        }
    }
    let tree = ProcessTree::build(
        &snapshot.processes,
        ProcessSort::ascending(ProcessSortKey::Pid),
    );
    let prefixes = tree_prefixes(GlyphSet::ascii(), &tree, 24);
    let rows: Vec<ProcessRow<'_>> = tree
        .rows()
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let process = snapshot.processes.get(row.process_index)?;
            Some(ProcessRow::new(process).with_prefix(prefixes.get(index)?))
        })
        .collect();
    let buffer = render(100, 7, |buffer, area| {
        let layout = TableLayout::for_area(area);
        ProcessTable::new(ascii(), &layout, &rows).render(area, buffer);
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn the_process_table_at_the_narrowest_supported_width() {
    // §5.7 keeps a stable minimal process list down to 60x16.
    let snapshot = sample_at(Scenario::default(), 3);
    let buffer = render(60, 7, |buffer, area| {
        table_only(
            buffer,
            area,
            ascii(),
            &snapshot.processes,
            snapshot.processes.first().map(|process| process.identity),
        );
    });
    insta::assert_snapshot!(text_of(&buffer));
}

#[test]
fn notable_process_states_are_visibly_distinct() {
    // §7.2: zombie and uninterruptible-sleep rows. The marker column carries the
    // state code so the cue survives the STATE column being dropped.
    use monitrs_core::model::ProcessState;

    let scenario = Scenario {
        processes: vec![
            FakeProcess::new(100, 1, "healthy", "healthy").with_cpu(Pattern::Steady(3.0)),
            FakeProcess::new(200, 2, "defunct", "defunct")
                .with_state(ProcessState::Zombie)
                .with_cpu(Pattern::Steady(0.0)),
            FakeProcess::new(300, 3, "blocked-on-nfs", "blocked-on-nfs")
                .with_state(ProcessState::UninterruptibleSleep)
                .with_cpu(Pattern::Steady(0.0)),
        ],
        ..Scenario::default()
    };
    let snapshot = sample_at(scenario, 3);
    let wide = render(100, 4, |buffer, area| {
        table_only(buffer, area, no_color(), &snapshot.processes, None);
    });
    let narrow = render(30, 4, |buffer, area| {
        table_only(buffer, area, no_color(), &snapshot.processes, None);
    });
    let mut combined = text_of(&wide);
    combined.push_str("---\n");
    combined.push_str(&text_of(&narrow));
    insta::assert_snapshot!(combined);
}

#[test]
fn the_meter_at_every_interesting_width() {
    // The degradation ladder: bar, no bar, no value, nothing.
    let snapshot = sample_at(Scenario::default(), 20);
    let cpu = snapshot.cpu.total.map(|usage| usage.busy);
    let buffer = render(48, 12, |buffer, area| {
        let presentation = ascii();
        for (index, width) in [48u16, 32, 20, 14, 9, 6, 3, 1].into_iter().enumerate() {
            let Ok(y) = u16::try_from(index) else { break };
            Meter::new(presentation, cpu)
                .with_label("CPU")
                .with_label_width(4)
                .render(Rect::new(area.x, area.y + y, width, 1), buffer);
        }
        for (index, width) in [48u16, 20, 9, 3].into_iter().enumerate() {
            let Ok(y) = u16::try_from(index + 8) else {
                break;
            };
            Meter::new(presentation, MetricState::<Percent>::PermissionDenied)
                .with_label("MEM")
                .with_label_width(4)
                .render(Rect::new(area.x, area.y + y, width, 1), buffer);
        }
    });
    insta::assert_snapshot!(text_of(&buffer));
}
