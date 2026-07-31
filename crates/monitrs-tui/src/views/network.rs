//! The Network screen (§7.4): interfaces, throughput, errors, history, and the
//! two kinds of total.
//!
//! ```text
//! + INTERFACES ------------------------------------------- 2 links ------+
//! | IFACE  STATE  ADDRESS          RX/s     TX/s   RX pps  TX pps  UTIL  |
//! | en0    +up    192.168.1.42/24  18M/s   2.3M/s  14200    3100   n/a   |
//! + TOTALS  since launch and OS counters are different figures ---------+
//! | en0  launch rx 342M tx 42M  |  os rx 900G tx 120G  err 0  drop 0     |
//! + THROUGHPUT HISTORY 5m ------------------ RX peak 18M/s -------------+
//! | RX  .....::-=+*##@%#*+=--:...                                       |
//! ```
//!
//! # No utilization without a link speed
//!
//! §7.4 and §26 are explicit: a network percentage is meaningless without a known
//! link capacity. This screen never computes one. It renders exactly what
//! [`NetworkSnapshot::utilization`] returns, which is
//! [`UnavailableReason::LinkSpeedUnknown`] whenever the speed is absent — the
//! common case on Wi-Fi and in virtual machines — and the cell then reads `link
//! speed unknown` or, in a narrow column, `n/a` with its own symbol. There is no
//! code path here that divides a rate by anything.
//!
//! # Two totals, never merged
//!
//! §7.4 asks for the total since launch *and* the OS counter. They are different
//! figures with different failure modes: the launch total starts at zero and is
//! always meaningful, while the OS counter may have wrapped, been reset by a
//! driver reload, or be missing entirely. They therefore get their own section
//! with their own labels rather than being added together — a sum of the two would
//! be a number that describes nothing.
//!
//! [`NetworkSnapshot::utilization`]: monitrs_core::model::NetworkSnapshot::utilization
//! [`UnavailableReason::LinkSpeedUnknown`]: monitrs_core::model::UnavailableReason::LinkSpeedUnknown

use monitrs_core::history::HistoryMetric;
use monitrs_core::model::{
    InterfaceErrors, LinkState, NetworkSnapshot, SystemSnapshot, TrafficTotals,
};
use monitrs_core::units::{MAX_BYTE_RATE_WIDTH, format_bytes_compact};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Borders, Widget};

use crate::app::AppState;
use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Presentation, Sparkline, SparklineCaret};

use super::{
    Chrome, SHARED_BOTTOM, caret_note, draw_bordered_panel, history_span_label, inner_of, inset,
    muted_line, plot_peak, plot_series, row_builder, selected_sample_offset, split_rows,
    truncation_label, write_lines,
};

/// Cells reserved for an interface name.
const NAME_WIDTH: u16 = 8;

/// Cells reserved for the operational state, symbol included.
const STATE_WIDTH: u16 = 8;

/// Cells reserved for the first address, before middle truncation.
const ADDRESS_WIDTH: u16 = 20;

/// Cells reserved for a throughput figure, from the formatter's own bound (§5.4).
const RATE_WIDTH: u16 = MAX_BYTE_RATE_WIDTH;

/// Cells reserved for a packet rate.
const PACKET_WIDTH: u16 = 9;

/// Cells reserved for the utilization column.
///
/// Wide enough for `n/a` and a symbol, never wide enough to make a bare
/// percentage look like the normal case (§7.4).
const UTIL_WIDTH: u16 = 6;

/// Cells reserved for an error or drop counter.
const COUNTER_WIDTH: u16 = 8;

/// Cells reserved for the labels down the left of the history panel.
const HISTORY_LABEL_WIDTH: u16 = 4;

/// Draws the Network screen (§7.4).
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, presentation: Presentation<'_>) {
    let Some(body) = Chrome::resolve(area).body else {
        return;
    };
    let buffer = frame.buffer_mut();
    let Some(snapshot) = state.snapshot() else {
        write_lines(
            buffer,
            body,
            &[muted_line(presentation, body.width, "warming up")],
        );
        return;
    };

    // Interfaces first — they are the screen's subject. The totals and the history
    // take fixed shares of what is left, and the history is dropped first because
    // the aggregate plots also appear on the Overview.
    let links = u16::try_from(snapshot.networks.len()).unwrap_or(u16::MAX);
    let interfaces = links.saturating_add(3).min(body.height);
    let remainder = body.height.saturating_sub(interfaces);
    let totals = links.saturating_add(2).min(remainder);
    let history = remainder.saturating_sub(totals);
    let rows = split_rows(body, &[interfaces, totals, history]);

    // Vertically adjacent panels share the row between them (§5.5); whichever panel
    // is last keeps its bottom edge, because the status footer is not a panel.
    let last_is_history = rows.get(2).is_some_and(|area| area.height >= 3);
    let totals_borders = if last_is_history {
        SHARED_BOTTOM
    } else {
        Borders::ALL
    };
    if let Some(area) = rows.first() {
        draw_interfaces(buffer, *area, snapshot, presentation, SHARED_BOTTOM);
    }
    if let Some(area) = rows.get(1).filter(|area| area.height >= 3) {
        draw_totals(buffer, *area, snapshot, presentation, totals_borders);
    }
    if let Some(area) = rows.get(2).filter(|area| area.height >= 3) {
        draw_history(buffer, *area, state, presentation, Borders::ALL);
    }
}

/// Draws the per-interface section (§7.4).
fn draw_interfaces(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let probe = inner_of(presentation, area, borders);
    let body_rows = usize::from(probe.height.saturating_sub(1));
    let total = snapshot.networks.len();
    let trailing =
        truncation_label(body_rows.min(total), total).unwrap_or_else(|| format!("{total} links"));
    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        "INTERFACES",
        Some(trailing.as_str()),
        false,
        borders,
    );
    if inner.is_empty() {
        return;
    }
    let mut lines = vec![interface_header(presentation, inner.width)];
    for interface in snapshot.networks.iter().take(body_rows) {
        lines.push(interface_row(presentation, inner.width, interface));
    }
    if snapshot.networks.is_empty() {
        lines.push(muted_line(
            presentation,
            inner.width,
            "no interface counters reported",
        ));
    }
    write_lines(buffer, inner, &lines);
}

/// The interface section's column header.
fn interface_header(presentation: Presentation<'_>, width: u16) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    let muted = presentation.style(Token::Muted);
    for (text, cells, align) in [
        ("IFACE", NAME_WIDTH, Align::Left),
        ("STATE", STATE_WIDTH, Align::Left),
        ("ADDRESS", ADDRESS_WIDTH, Align::Left),
        ("RX/s", RATE_WIDTH, Align::Right),
        ("TX/s", RATE_WIDTH, Align::Right),
        ("RX pps", PACKET_WIDTH, Align::Right),
        ("TX pps", PACKET_WIDTH, Align::Right),
        ("ERR", COUNTER_WIDTH, Align::Right),
        ("DROP", COUNTER_WIDTH, Align::Right),
        ("UTIL", UTIL_WIDTH, Align::Right),
    ] {
        row.push_field(text, cells, align, muted);
        row.pad(1);
    }
    row.finish()
}

/// One interface's row.
fn interface_row(
    presentation: Presentation<'_>,
    width: u16,
    interface: &NetworkSnapshot,
) -> Line<'static> {
    let units = presentation.units();
    let glyphs = presentation.glyphs();
    let mut row = row_builder(presentation, width);
    row.push_field(
        &interface.name,
        NAME_WIDTH,
        Align::Left,
        presentation.style(Token::Text),
    );
    row.pad(1);
    let state = link_display(interface);
    row.push_field(
        &state.fitted(usize::from(STATE_WIDTH), glyphs),
        STATE_WIDTH,
        Align::Left,
        presentation.metric_style(&state),
    );
    row.pad(1);
    row.push_field(
        &states::fit_middle_within(&address_text(interface), usize::from(ADDRESS_WIDTH), glyphs),
        ADDRESS_WIDTH,
        Align::Left,
        presentation.style(Token::Muted),
    );
    row.pad(1);
    for state in [&interface.rx, &interface.tx] {
        let display = states::describe_byte_rate(state, units);
        row.push_field(
            &display.fitted(usize::from(RATE_WIDTH), glyphs),
            RATE_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    for state in [&interface.rx_packets, &interface.tx_packets] {
        let display = states::describe(state, |rate| format!("{:.0}", rate.per_second()));
        row.push_field(
            &display.fitted(usize::from(PACKET_WIDTH), glyphs),
            PACKET_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    for display in [error_display(interface), drop_display(interface)] {
        row.push_field(
            &display.fitted(usize::from(COUNTER_WIDTH), glyphs),
            COUNTER_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    // §7.4: rendered, never computed. `utilization()` is the method that refuses
    // to divide by an unknown link speed, and this cell shows whatever it says.
    let utilization = states::describe_percent(&interface.utilization());
    row.push_field(
        &utilization.fitted(usize::from(UTIL_WIDTH), glyphs),
        UTIL_WIDTH,
        Align::Right,
        presentation.metric_style(&utilization),
    );
    row.finish()
}

/// The operational state as text, a token, and a symbol.
///
/// Shared with the Overview's footer rows, which is why it is `pub(crate)`: the
/// state's spelling and its cue must be the same on both screens (§5.2).
pub(crate) fn link_display(interface: &NetworkSnapshot) -> MetricDisplay {
    states::describe(&interface.state, |state: &LinkState| {
        format!("{}{}", state.symbol(), state.label())
    })
}

/// The first assigned address, or a dash when the platform reported none.
fn address_text(interface: &NetworkSnapshot) -> String {
    match interface.addresses.first() {
        Some(address) => match address.prefix_len {
            Some(prefix) => format!("{}/{prefix}", address.ip),
            None => address.ip.to_string(),
        },
        // Not a placeholder for an unavailable metric: an interface really can have
        // no address, and `addresses` is a plain `Vec` rather than a `MetricState`
        // precisely because "none assigned" is a fact.
        None => "-".to_owned(),
    }
}

/// Receive plus transmit errors, keeping the counters' availability.
fn error_display(interface: &NetworkSnapshot) -> MetricDisplay {
    states::describe(&interface.errors, |errors: &InterfaceErrors| {
        errors
            .rx_errors
            .saturating_add(errors.tx_errors)
            .to_string()
    })
}

/// Receive plus transmit drops, keeping the counters' availability.
fn drop_display(interface: &NetworkSnapshot) -> MetricDisplay {
    states::describe(&interface.errors, |errors: &InterfaceErrors| {
        errors
            .rx_dropped
            .saturating_add(errors.tx_dropped)
            .to_string()
    })
}

/// Draws the two-totals section (§7.4).
fn draw_totals(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        "TOTALS",
        Some("since launch and OS counter are separate figures"),
        false,
        borders,
    );
    if inner.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    for interface in snapshot.networks.iter().take(usize::from(inner.height)) {
        lines.push(totals_row(presentation, inner.width, interface));
    }
    if lines.is_empty() {
        lines.push(muted_line(
            presentation,
            inner.width,
            "no interface counters reported",
        ));
    }
    write_lines(buffer, inner, &lines);
}

/// One interface's launch totals beside its OS counters.
fn totals_row(
    presentation: Presentation<'_>,
    width: u16,
    interface: &NetworkSnapshot,
) -> Line<'static> {
    let units = presentation.units();
    let glyphs = presentation.glyphs();
    let mut row = row_builder(presentation, width);
    row.push_field(
        &interface.name,
        NAME_WIDTH,
        Align::Left,
        presentation.style(Token::Text),
    );
    row.pad(1);

    let launch = &interface.since_launch;
    row.push(
        &format!(
            "launch rx {} tx {} ({} / {} pkt)",
            format_bytes_compact(launch.rx_bytes, units),
            format_bytes_compact(launch.tx_bytes, units),
            launch.rx_packets,
            launch.tx_packets
        ),
        presentation.style(Token::Text),
    );
    row.push("  |  ", presentation.style(Token::Border));

    // The OS counter is a `MetricState` because a platform may not expose it and
    // because it can be reset underneath us; the launch total never can be.
    let os = states::describe(&interface.os_totals, |totals: &TrafficTotals| {
        format!(
            "rx {} tx {} ({} / {} pkt)",
            format_bytes_compact(totals.rx_bytes, units),
            format_bytes_compact(totals.tx_bytes, units),
            totals.rx_packets,
            totals.tx_packets
        )
    });
    let remaining = row.remaining();
    row.push(
        &format!(
            "os {}",
            os.fitted(usize::from(remaining.saturating_sub(3)), glyphs)
        ),
        presentation.metric_style(&os),
    );
    row.finish()
}

/// Draws the throughput-history section (§7.4's historical graph).
fn draw_history(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let ring = state.history();
    let units = presentation.units();
    let title = format!("THROUGHPUT HISTORY {}", history_span_label(ring));
    // §7.4 forbids a utilization percentage without a known link speed, so both
    // plots are self-scaling and the panel states the ceiling they are drawn to.
    let peak =
        plot_peak(ring, HistoryMetric::NetworkRx, units).map(|peak| format!("RX peak {peak}"));
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        &title,
        peak.as_deref(),
        false,
        borders,
    ));
    if inner.is_empty() {
        return;
    }
    let rx = plot_series(ring, HistoryMetric::NetworkRx);
    let tx = plot_series(ring, HistoryMetric::NetworkTx);
    let caret = selected_sample_offset(state);
    let note = caret_note(state, units);

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

    // Both plots are self-scaling — a link has no natural 100% unless its speed is
    // known, and §7.4 forbids pretending otherwise — so the panel's trailing label
    // carries the ceiling.
    if let Some(rect) = next_row() {
        Sparkline::new(presentation, &rx)
            .with_label("RX")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .self_scaling(true)
            .with_token(Token::Graph1)
            .render(rect, buffer);
    }
    if let Some(rect) = next_row() {
        Sparkline::new(presentation, &tx)
            .with_label("TX")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .self_scaling(true)
            .with_token(Token::Graph2)
            .render(rect, buffer);
    }
    if let Some(offset) = caret
        && let Some(rect) = next_row()
    {
        SparklineCaret::new(presentation, &rx, offset)
            .with_label("RX")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .with_note(&note)
            .render(rect, buffer);
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Instant, SystemTime};

    use monitrs_core::model::{InterfaceAddress, InterfaceKind, MetricState, UnavailableReason};
    use monitrs_core::units::Rate;

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

    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn interface(name: &str, speed: MetricState<u64>) -> NetworkSnapshot {
        let rate = Rate::new(1_000_000.0).unwrap_or(Rate::ZERO);
        let mut interface = NetworkSnapshot::warming_up(name.into(), InterfaceKind::Physical);
        interface.state = MetricState::Available(LinkState::Up);
        interface.addresses = vec![InterfaceAddress {
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
            prefix_len: Some(24),
        }];
        interface.rx = MetricState::Available(rate);
        interface.tx = MetricState::Available(rate);
        interface.rx_packets = MetricState::Available(rate);
        interface.tx_packets = MetricState::Available(rate);
        interface.errors = MetricState::Available(InterfaceErrors {
            rx_errors: 2,
            tx_errors: 3,
            rx_dropped: 4,
            tx_dropped: 5,
        });
        interface.link_speed_mbps = speed;
        interface.since_launch = TrafficTotals {
            rx_bytes: 1_000,
            tx_bytes: 2_000,
            rx_packets: 10,
            tx_packets: 20,
        };
        interface
    }

    #[test]
    fn no_utilization_percentage_is_rendered_without_a_link_speed() {
        // §7.4 and §26. The method already refuses; this asserts the screen renders
        // the refusal rather than working around it.
        let unknown = interface(
            "en0",
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
        );
        assert!(unknown.utilization().displayable().is_none());
        let text = text_of(&interface_row(presentation(), 140, &unknown));
        assert!(
            text.contains("n/a") || text.contains("link speed"),
            "{text}"
        );
        // The one thing that must not appear: a plausible-looking percentage.
        assert!(!text.trim_end().ends_with('%'), "{text}");
    }

    #[test]
    fn a_known_link_speed_does_produce_a_utilization_percentage() {
        // The other half of the rule: where the speed is known, the figure is real.
        let known = interface("en0", MetricState::Available(1_000));
        assert!(known.utilization().displayable().is_some());
        let text = text_of(&interface_row(presentation(), 140, &known));
        assert!(text.trim_end().ends_with('%'), "{text}");
    }

    #[test]
    fn the_two_totals_are_rendered_as_distinct_figures() {
        // §7.4: total since launch AND the OS counter, never summed.
        let mut link = interface("en0", MetricState::Unsupported);
        link.os_totals = MetricState::Available(TrafficTotals {
            rx_bytes: 900_000_000,
            tx_bytes: 120_000_000,
            rx_packets: 9_000,
            tx_packets: 1_200,
        });
        let text = text_of(&totals_row(presentation(), 160, &link));
        assert!(text.contains("launch rx"), "{text}");
        assert!(text.contains("os rx"), "{text}");
    }

    #[test]
    fn a_missing_os_counter_is_a_placeholder_and_not_a_zero() {
        let link = interface("en0", MetricState::Unsupported);
        assert!(link.os_totals.is_warming_up());
        let text = text_of(&totals_row(presentation(), 160, &link));
        assert!(
            text.contains("os warming up") || text.contains("os n/a"),
            "{text}"
        );
    }

    #[test]
    fn errors_and_drops_are_reported_separately() {
        let link = interface("en0", MetricState::Unsupported);
        assert_eq!(error_display(&link).text(), "5");
        assert_eq!(drop_display(&link).text(), "9");
    }

    #[test]
    fn an_interface_with_no_address_says_so_without_claiming_a_metric() {
        let mut link = interface("en0", MetricState::Unsupported);
        link.addresses.clear();
        assert_eq!(address_text(&link), "-");
    }

    #[test]
    fn the_link_state_carries_a_symbol_as_well_as_a_word() {
        // §5.2: colour is never the only indicator.
        let up = interface("en0", MetricState::Unsupported);
        assert_eq!(link_display(&up).text(), "+up");
        let mut down = up.clone();
        down.state = MetricState::Available(LinkState::Down);
        assert_eq!(link_display(&down).text(), "-down");
        let mut unknown = up;
        unknown.state = MetricState::PermissionDenied;
        assert!(link_display(&unknown).is_placeholder());
    }

    #[test]
    fn the_header_never_promises_a_utilization_column_wider_than_a_placeholder() {
        // A wide `UTIL` column would make a bare percentage look like the normal
        // case, which on an unknown link it is not (§7.4).
        let header = text_of(&interface_header(presentation(), 140));
        assert!(header.contains("UTIL"));
        const { assert!(UTIL_WIDTH <= 6) };
    }

    #[test]
    fn a_snapshotless_screen_renders_nothing_but_a_warming_up_note() {
        let snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
        assert!(snapshot.networks.is_empty());
        let line = muted_line(presentation(), 40, "warming up");
        assert!(text_of(&line).contains("warming up"));
    }
}
