//! The Overview screen (§7.1), composed as §5.5's dashboard.
//!
//! ```text
//! + PRESSURE ------------------+- HISTORY 5m ---- I/O peak 42M/s -+
//! | . CPU  normal      37%     | CPU  .....::-=+*##@%#*+=--:...   |
//! | ! MEM  watch       71%     | MEM  ====+++++*********########  |
//! | . DISK normal    60M/s     | I/O  .......:==#@@*=:........    |
//! | temp 62.5C  bat 82%-       | CORE #-#-#-#-                    |
//! + PROCESSES ------------------------------------- 218 total ----+
//! ```
//!
//! Every rectangle comes from [`Layout`]; this module only decides what goes in
//! them. Four decisions are worth stating because they are not obvious from the
//! code:
//!
//! * **Per-core utilization is a strip, never rows.** §7.1 forbids "rendering
//!   hundreds of rows" for a 256-thread machine, and [`CoreStrip`] aggregates by
//!   group maximum once the cores outnumber the cells. It is one row at every core
//!   count.
//! * **The radar may lose signals; it never loses them quietly.** The pressure
//!   panel is six rows in the `Wide` band whatever the terminal height, so a
//!   platform with all nine §2.3 signals cannot show them all. The most severe are
//!   kept ([`Radar::with_severe_first`]), platform-impossible ones are dropped
//!   ([`Radar::hide_unsupported`], which §4 permits when space is scarce), and the
//!   panel's trailing label says how many of how many are on screen.
//! * **There is no separate sensors row.** §7.1's optional temperature and battery
//!   summary lives in the header's meter notes instead, at every band that draws
//!   this screen (see `draw_pressure`'s own comment, below) — a radar row would
//!   cost a §2.3 pressure signal to repeat a reading the header already shows, and
//!   the header can carry a carried-over, aged reading (`~00:28`) that a radar
//!   row's `n/a`/available split cannot.
//! * **The I/O plot is self-scaling, so the panel states its ceiling.** A byte
//!   rate has no natural 100%, and a plot drawn against an unstated scale is not a
//!   measurement — so the peak goes in the panel's trailing label where changing
//!   it cannot move the plots (§5.4).
//!
//! [`Layout`]: crate::layout::Layout
//! [`CoreStrip`]: crate::widgets::CoreStrip
//! [`Radar`]: crate::widgets::Radar

use monitrs_core::history::HistoryMetric;
use monitrs_core::model::{MetricState, NetworkSnapshot, PressureSnapshot};
use monitrs_core::units::{ByteUnits, MAX_BYTE_RATE_WIDTH, Percent, Rate};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Borders, Widget};

use crate::app::{AppState, PanelFocus};
use crate::layout::{Align, Breakpoint};
use crate::theme::Token;
use crate::widgets::states;
use crate::widgets::{CoreStrip, Presentation, Radar, Sparkline, SparklineCaret};

use super::{
    Chrome, SHARED_BOTTOM, SHARED_RIGHT, SHARED_RIGHT_AND_BOTTOM, aggregate_disk_rates,
    aggregate_network_rates, caret_note, draw_bordered_panel, history_span_label, inner_of, inset,
    muted_line, network, plot_peak, plot_series, processes, row_builder, selected_sample_offset,
    truncation_label, write_lines,
};

/// Cells reserved for the labels down the left of the history panel, so the
/// sparklines, the caret, and the core strip all start their plots in the same
/// column (§5.5's history block reads as one timeline).
const HISTORY_LABEL_WIDTH: u16 = 5;

/// Cells reserved for the leading label of a footer rate row.
const RATE_LABEL_WIDTH: u16 = 6;

/// Cells reserved for one rate value in the footer.
///
/// Taken from the formatter's own bound so a value crossing a unit boundary
/// cannot move the field beside it (§5.4).
const RATE_VALUE_WIDTH: u16 = MAX_BYTE_RATE_WIDTH;

/// The narrowest radar that also has room for a severity bar.
const RADAR_BAR_WIDTH: u16 = 44;

/// Draws the Overview screen into the body of `area`.
///
/// `area` is the whole frame; the chrome has already been drawn by
/// [`super::render`], and [`Chrome::resolve`] is how this function finds the
/// panels it owns.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, presentation: Presentation<'_>) {
    let chrome = Chrome::resolve(area);
    let layout = chrome.layout;
    let focus = state.focus();
    let compact = chrome.breakpoint() == Breakpoint::Compact;
    let buffer = frame.buffer_mut();

    // §5.5 shares the border between vertically and horizontally adjacent panels:
    // one row is simultaneously the bottom of the radar and the top of the process
    // table, and one column is both the radar's right edge and the history panel's
    // left. Omitting the duplicate is what buys the rows the mockup spends on data.
    let lower_panel = layout.pins.is_some() || (layout.summary.is_some() && !compact);
    let table_borders = if lower_panel {
        SHARED_BOTTOM
    } else {
        Borders::ALL
    };

    if let Some(pressure) = layout.pressure {
        draw_pressure(
            buffer,
            pressure,
            state,
            presentation,
            focus == PanelFocus::Pressure,
            SHARED_RIGHT_AND_BOTTOM,
        );
    }
    if let Some(history) = layout.history {
        draw_history(
            buffer,
            history,
            state,
            presentation,
            focus == PanelFocus::History,
            SHARED_BOTTOM,
        );
    }
    // §5.7's `Standard` band shows "one lower summary panel selected by focus".
    // In `Compact` the same slot is the chrome's one-line summary strip, which
    // `Chrome::resolve` has already claimed.
    if let Some(summary) = layout.summary.filter(|_| !compact) {
        draw_focus_summary(buffer, summary, state, presentation);
    }
    if let Some(table) = layout.processes {
        processes::draw_table_panel(
            buffer,
            table,
            state,
            presentation,
            focus == PanelFocus::Processes,
            table_borders,
        );
    }
    if let Some(pins) = layout.pins {
        processes::draw_pins_panel(
            buffer,
            pins,
            state,
            presentation,
            focus == PanelFocus::Pins,
            SHARED_RIGHT,
        );
    }
    if let Some(rates) = layout.network {
        draw_rates(
            buffer,
            rates,
            state,
            presentation,
            focus == PanelFocus::Network,
            Borders::ALL,
        );
    }
}

/// An empty radar, for the frames before the first snapshot arrives.
fn warming_up_pressure() -> PressureSnapshot {
    PressureSnapshot {
        signals: Vec::new(),
        psi: MetricState::WarmingUp,
    }
}

/// Draws the Pressure Radar panel (§2.3).
///
/// §7.1's optional temperature and battery summary is deliberately *not* here: the
/// header's meter notes already carry it at every band that draws this panel, and
/// the radar rows are worth more than a repetition — the panel is six rows in the
/// `Wide` band whatever the terminal height, so every row it gives up is a §2.3
/// signal the reader does not see.
fn draw_pressure(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    focused: bool,
    borders: Borders,
) {
    let fallback = warming_up_pressure();
    let pressure = state
        .snapshot()
        .map_or(&fallback, |snapshot| &snapshot.pressure);

    // The interior is measured before the panel is drawn, because the trailing
    // label has to state the truncation that the interior height causes.
    let probe = inset(inner_of(presentation, area, borders));
    let radar = Radar::new(presentation, &pressure.signals)
        .with_severe_first(true)
        .hide_unsupported(true)
        .with_bars(probe.width >= RADAR_BAR_WIDTH);
    let label = truncation_label(
        radar.visible_signals(probe.height),
        radar.ordered_signals().len(),
    );

    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        "PRESSURE",
        label.as_deref(),
        focused,
        borders,
    ));
    if inner.is_empty() {
        return;
    }
    radar.render(inner, buffer);
}

/// Draws the rolling-history panel and, when the Time Lens is engaged, its caret.
fn draw_history(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    focused: bool,
    borders: Borders,
) {
    let ring = state.history();
    let units = presentation.units();
    let title = format!("HISTORY {}", history_span_label(ring));
    // A self-scaling plot owes the reader its ceiling, and the trailing label is
    // where changing it cannot move the plots (§5.4).
    let peak =
        plot_peak(ring, HistoryMetric::DiskWrite, units).map(|peak| format!("I/O peak {peak}"));
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        &title,
        peak.as_deref(),
        focused,
        borders,
    ));
    if inner.is_empty() {
        return;
    }

    let cpu = plot_series(ring, HistoryMetric::CpuBusy);
    let memory = plot_series(ring, HistoryMetric::MemoryUsedShare);
    let disk = plot_series(ring, HistoryMetric::DiskWrite);
    let cores = per_core_series(state);
    let caret = selected_sample_offset(state);
    let note = caret_note(state);

    let mut used = 0u16;
    let mut next_row = || -> Option<Rect> {
        if used >= inner.height {
            return None;
        }
        let rect = Rect {
            y: inner.y.saturating_add(used),
            height: 1,
            ..inner
        };
        used = used.saturating_add(1);
        Some(rect)
    };

    if let Some(rect) = next_row() {
        Sparkline::new(presentation, &cpu)
            .with_label("CPU")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .with_token(Token::Graph1)
            .render(rect, buffer);
    }
    if let Some(rect) = next_row() {
        Sparkline::new(presentation, &memory)
            .with_label("MEM")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .with_token(Token::Graph2)
            .render(rect, buffer);
    }
    if let Some(rect) = next_row() {
        Sparkline::new(presentation, &disk)
            .with_label("I/O")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .self_scaling(true)
            .with_token(Token::Graph3)
            .render(rect, buffer);
    }
    // §2.1 and §26: while the timeline is frozen the caret is the row that makes
    // the panel unmistakably historical, so it outranks the core strip.
    if let Some(offset) = caret
        && let Some(rect) = next_row()
    {
        SparklineCaret::new(presentation, &cpu, offset)
            .with_label("CPU")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .with_note(&note)
            .render(rect, buffer);
    }
    if let Some(rect) = next_row() {
        CoreStrip::new(presentation, &cores)
            .with_label("CORE")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .with_count(false)
            .with_token(Token::Graph4)
            .render(rect, buffer);
    }
}

/// Per-logical-CPU utilization as a [`CoreStrip`] series.
///
/// An unavailable per-core read becomes one unavailable entry per logical CPU
/// rather than an empty list: §4 forbids collapsing "not measured" into "no
/// cores", and an empty strip would read as a machine with no CPUs.
fn per_core_series(state: &AppState) -> Vec<MetricState<Percent>> {
    let Some(snapshot) = state.snapshot() else {
        return Vec::new();
    };
    match snapshot.cpu.per_core.displayable() {
        Some((cores, age)) if !cores.is_empty() => cores
            .iter()
            .map(|core| {
                if age.is_zero() {
                    MetricState::Available(core.busy)
                } else {
                    // §4: a retained value stays marked stale all the way to the
                    // cell, which is what keeps the strip from claiming a fresh
                    // reading it does not have.
                    MetricState::Stale {
                        value: core.busy,
                        age,
                    }
                }
            })
            .collect(),
        _ => (0..snapshot.cpu.logical_count)
            .map(|_| snapshot.cpu.per_core.as_ref().map(|_| Percent::ZERO))
            .collect(),
    }
}

/// Draws §5.7's focus-selected lower summary panel for the `Standard` band.
///
/// Which panel appears is decided by [`AppState::focus`], so `Tab` cycles the
/// lower panel exactly as §5.7 describes. The pressure radar is the fallback,
/// because a dashboard whose focus happens to sit on the process table should
/// still show the machine's health.
fn draw_focus_summary(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    match state.focus() {
        PanelFocus::Pins => {
            processes::draw_pins_panel(buffer, area, state, presentation, true, Borders::ALL);
        }
        PanelFocus::Network => draw_rates(buffer, area, state, presentation, true, Borders::ALL),
        PanelFocus::History => draw_history(buffer, area, state, presentation, true, Borders::ALL),
        PanelFocus::Pressure | PanelFocus::Summary | PanelFocus::Processes => {
            draw_pressure(buffer, area, state, presentation, false, Borders::ALL);
        }
    }
}

/// Draws the aggregate disk and network rate panel of §7.1.
///
/// The aggregates come first because they are what §7.1 asks for; per-interface
/// rows follow while there are cells for them, which is what §5.5's `NETWORK`
/// footer shows.
fn draw_rates(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    focused: bool,
    borders: Borders,
) {
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        "NETWORK",
        None,
        focused,
        borders,
    ));
    if inner.is_empty() {
        return;
    }
    let Some(snapshot) = state.snapshot() else {
        write_lines(
            buffer,
            inner,
            &[muted_line(presentation, inner.width, "no sample yet")],
        );
        return;
    };
    let units = presentation.units();
    let (rx, tx) = aggregate_network_rates(snapshot);
    let (read, write) = aggregate_disk_rates(snapshot);
    let mut lines = vec![
        rate_line(presentation, inner.width, "NET", ("rx", rx), ("tx", tx)),
        rate_line(
            presentation,
            inner.width,
            "DISK",
            ("rd", read),
            ("wr", write),
        ),
    ];
    for interface in &snapshot.networks {
        if lines.len() >= usize::from(inner.height) {
            break;
        }
        lines.push(interface_line(presentation, inner.width, interface, units));
    }
    write_lines(buffer, inner, &lines);
}

/// `NET    rx  18M/s  tx 2.3M/s` — one aggregate with two directions.
fn rate_line(
    presentation: Presentation<'_>,
    width: u16,
    label: &str,
    first: (&str, MetricState<Rate>),
    second: (&str, MetricState<Rate>),
) -> Line<'static> {
    let units = presentation.units();
    let mut row = row_builder(presentation, width);
    row.push_field(
        label,
        RATE_LABEL_WIDTH,
        Align::Left,
        presentation.style(Token::Text),
    );
    for (direction, state) in [first, second] {
        let display = states::describe_byte_rate(&state, units);
        row.push(direction, presentation.style(Token::Muted));
        // An explicit separator rather than one borrowed from the value field's
        // right-alignment padding: the field is exactly as wide as the widest rate
        // it can hold, so a value that uses all of it — `1023K/s`, or the
        // `warming` placeholder — would otherwise run straight into the `rx`.
        row.pad(1);
        row.push_field(
            &display.fitted(usize::from(RATE_VALUE_WIDTH), presentation.glyphs()),
            RATE_VALUE_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(2);
    }
    row.finish()
}

/// `en0   +up  rx  18M/s  tx 2.3M/s` — one interface's state and throughput.
fn interface_line(
    presentation: Presentation<'_>,
    width: u16,
    interface: &NetworkSnapshot,
    units: ByteUnits,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    row.push_field(
        &interface.name,
        RATE_LABEL_WIDTH,
        Align::Left,
        presentation.style(Token::Text),
    );
    let state = network::link_display(interface);
    row.push_field(
        &state.fitted(4, presentation.glyphs()),
        5,
        Align::Left,
        presentation.metric_style(&state),
    );
    for (direction, rate) in [("rx", interface.rx), ("tx", interface.tx)] {
        let display = states::describe_byte_rate(&rate, units);
        row.push(direction, presentation.style(Token::Muted));
        // See `rate_line`: the separator cannot come from the padding.
        row.pad(1);
        row.push_field(
            &display.fitted(usize::from(RATE_VALUE_WIDTH), presentation.glyphs()),
            RATE_VALUE_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(2);
    }
    row.finish()
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::sync::Arc;
    use std::time::{Instant, SystemTime};

    use monitrs_core::model::{
        InterfaceKind, MeasuredValue, Measurement, PressureId, PressureSignal, PressureState,
        SystemSnapshot,
    };

    use super::*;
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("a finite non-negative percentage")
    }

    fn signal(id: PressureId, state: PressureState) -> PressureSignal {
        PressureSignal {
            id,
            state: MetricState::Available(state),
            severity: MetricState::Available(percent(50.0)),
            raw: Some(Measurement::new(
                "busy",
                MeasuredValue::Percent(percent(50.0)),
            )),
            rule: "test rule",
            held_for: Some(Duration::from_secs(3)),
        }
    }

    /// The order the radar would draw `pressure` in.
    fn signal_order(pressure: &PressureSnapshot) -> Vec<PressureId> {
        Radar::new(presentation(), &pressure.signals)
            .with_severe_first(true)
            .hide_unsupported(true)
            .ordered_signals()
            .into_iter()
            .map(|signal| signal.id)
            .collect()
    }

    fn state_with(snapshot: SystemSnapshot) -> AppState {
        let mut state = AppState::default();
        let _ = crate::app::apply(
            &mut state,
            crate::event::Event::<()>::Snapshot(Arc::new(snapshot)),
        );
        state
    }

    #[test]
    fn the_calmest_signal_is_the_one_truncation_loses() {
        // §2.3: a critical signal must never be the row that got cut.
        let pressure = PressureSnapshot {
            signals: vec![
                signal(PressureId::Cpu, PressureState::Normal),
                signal(PressureId::Memory, PressureState::Critical),
                signal(PressureId::Disk, PressureState::Watch),
            ],
            psi: MetricState::Unsupported,
        };
        assert_eq!(
            signal_order(&pressure),
            vec![PressureId::Memory, PressureId::Disk, PressureId::Cpu]
        );
    }

    #[test]
    fn a_platform_impossible_signal_is_dropped_rather_than_shown_blank() {
        // §4 permits hiding an `Unsupported` optional row when space is scarce; it
        // never permits hiding an unavailable one, which is information.
        let pressure = PressureSnapshot {
            signals: vec![
                signal(PressureId::Cpu, PressureState::Normal),
                PressureSignal::unsupported(PressureId::PsiIo, "Linux only"),
                PressureSignal::warming_up(PressureId::Swap, "awaiting samples"),
            ],
            psi: MetricState::Unsupported,
        };
        let order = signal_order(&pressure);
        assert!(order.contains(&PressureId::Cpu));
        assert!(
            order.contains(&PressureId::Swap),
            "a warming-up signal is not the same as an impossible one"
        );
        assert!(!order.contains(&PressureId::PsiIo));
    }

    #[test]
    fn an_unavailable_per_core_read_still_reports_one_entry_per_cpu() {
        // §4: "not measured" must not collapse into "no cores".
        let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 12);
        snapshot.cpu.per_core = MetricState::PermissionDenied;
        let state = state_with(snapshot);
        let series = per_core_series(&state);
        assert_eq!(series.len(), 12);
        assert!(series.iter().all(|core| core.displayable().is_none()));
    }

    #[test]
    fn a_stale_per_core_read_stays_marked_stale_in_the_strip() {
        use monitrs_core::model::CpuUsage;

        let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 2);
        snapshot.cpu.per_core = MetricState::Available(vec![
            CpuUsage::plain(percent(10.0)),
            CpuUsage::plain(percent(90.0)),
        ])
        .into_stale(Duration::from_secs(4));
        let state = state_with(snapshot);
        let series = per_core_series(&state);
        assert_eq!(series.len(), 2);
        assert!(
            series.iter().all(MetricState::is_stale),
            "§4: a retained value must reach the cell still marked stale"
        );
    }

    #[test]
    fn the_aggregate_network_rate_excludes_loopback() {
        // §7.4: loopback traffic appears on both directions of one interface, so
        // counting it would double local activity as link throughput.
        let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
        let rate = Rate::new(1_000.0).expect("finite");
        let mut physical = NetworkSnapshot::warming_up("en0".into(), InterfaceKind::Physical);
        physical.rx = MetricState::Available(rate);
        physical.tx = MetricState::Available(rate);
        let mut loopback = NetworkSnapshot::warming_up("lo0".into(), InterfaceKind::Loopback);
        loopback.rx = MetricState::Available(rate);
        loopback.tx = MetricState::Available(rate);
        snapshot.networks = vec![physical, loopback];

        let (rx, _) = aggregate_network_rates(&snapshot);
        assert_eq!(rx.fresh().map(|rate| rate.per_second()), Some(1_000.0));
    }

    #[test]
    fn the_radar_keeps_every_row_the_panel_has() {
        // The pressure panel no longer spends a row on the sensor summary, because
        // the header's meter notes already carry it: every row here is a §2.3 signal.
        let pressure = PressureSnapshot {
            signals: vec![
                signal(PressureId::Cpu, PressureState::Normal),
                signal(PressureId::Memory, PressureState::Watch),
                signal(PressureId::Disk, PressureState::Normal),
                signal(PressureId::Swap, PressureState::Normal),
            ],
            psi: MetricState::Unsupported,
        };
        let radar = Radar::new(presentation(), &pressure.signals)
            .with_severe_first(true)
            .hide_unsupported(true);
        assert_eq!(radar.visible_signals(4), 4);
        assert!(!radar.is_truncated(4));
        assert!(radar.is_truncated(3));
    }
}
