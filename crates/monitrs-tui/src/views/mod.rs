//! The seven screens of §7, and the header and status footer that frame them
//! (§5.5, §5.7).
//!
//! A *screen* is the layer above [`crate::widgets`]: it decides what belongs on
//! the terminal and which rectangle each piece gets. It owns no state, reads no
//! files, and calls no collector — every screen is a pure function of
//! `(&AppState, Presentation, Rect)`, which is what makes §17.3's snapshot tests
//! possible and what keeps §10.5's effect discipline intact.
//!
//! # The shape of a screen function
//!
//! ```ignore
//! pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, presentation: Presentation<'_>)
//! ```
//!
//! `area` is always the **whole terminal**, not the screen's body. A screen
//! re-derives its own geometry from [`Chrome::resolve`], which is a pure function
//! of the rectangle, rather than being handed a pre-cut body. Two reasons:
//!
//! * The Overview needs the *named* panels of [`Layout`] — pressure, history,
//!   pins, network — and those are only meaningful relative to the full frame.
//!   Handing it a body rectangle would force it to reconstruct them from a
//!   different origin, and the reconstruction would then be free to disagree
//!   with [`crate::app::PanelFocus::is_present`], which decides where `Tab` may
//!   move.
//! * Geometry resolution is cheap and pure, so resolving it twice — once in
//!   [`render`] to draw the chrome, once in the screen — costs nothing and
//!   removes a parameter that could be passed inconsistently.
//!
//! # What the chrome owns
//!
//! [`render`] draws the header (§5.5's title line plus the CPU and memory
//! meters), the one-line CPU/memory/load strip that replaces those meters in the
//! `Compact` band (§5.7), and the status footer (tab strip and key hints). It
//! then dispatches to the screen for [`AppState::view`]. Below 80×20 it draws
//! §5.7's minimal process list or the resize notice and dispatches to nothing:
//! that band's list must be *stable*, and a list that changed when the user
//! pressed `3` would not be.
//!
//! # Two rules that shape every screen in this module
//!
//! * **Unavailable is never zero.** No screen matches on [`MetricState`]. Every
//!   value reaches the terminal through [`crate::widgets::states`], which is the
//!   one place §4's placeholder text, token, and symbol are decided.
//! * **Historical state is unmistakable from live.** The header carries the
//!   [`crate::app::TimelineStatus`] badge in brackets with its own symbol and
//!   token, the
//!   history panel grows a caret row, and the footer gains the return-to-live
//!   hint. Three independent cues, none of them colour alone (§2.1, §5.2, §26).
//!
//! [`MetricState`]: monitrs_core::model::MetricState

pub mod battery;
pub mod cpu;
pub mod inspect;
pub mod network;
pub mod overlays;
pub mod overview;
pub mod processes;
pub mod storage;

use core::time::Duration;
use std::time::SystemTime;

use monitrs_core::history::{HistoricalSample, HistoryMetric, HistoryRing, MetricComparison};
use monitrs_core::model::{
    BatterySnapshot, LoadSnapshot, MeasuredValue, MetricState, SensorSnapshot, SystemSnapshot,
    TemperatureReading,
};
use monitrs_core::units::{
    ByteUnits, Percent, Rate, display_width, format_age, format_bytes_compact, format_duration,
    format_uptime,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Borders, Widget};

use crate::action::{Action, ViewId};
use crate::app::AppState;
use crate::layout::{Breakpoint, Layout, unusable_notice};
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Meter, Painter, Panel, Presentation, RowBuilder};

/// Cells reserved for the meter labels in the header, so `CPU` and `MEM` align.
const HEADER_LABEL_WIDTH: u16 = 4;

/// Cells a header meter keeps for itself before its note may take any.
///
/// The label, the one-cell availability symbol, the value field, and the narrowest
/// useful bar all have to survive whatever the note asks for (§5.4: the bar is
/// reserved from geometry, never from the note's current length).
const HEADER_NOTE_RESERVE: u16 = HEADER_LABEL_WIDTH + 1 + 4 + 6;

/// Cells between two segments of a one-line summary or hint strip.
const SEGMENT_GAP: usize = 2;

/// Cells kept clear at the right-hand end of a panel's top rule, matching the
/// `--- 218 total ---+` form of §5.5.
const RULE_TAIL: u16 = 3;

/// The narrowest hint gap worth writing a status-line notice into.
///
/// Below this, a notice would be reduced to a word and a marker, which reads as
/// noise rather than as a message; the Inspect screen shows the full log instead.
const MIN_NOTICE_WIDTH: u16 = 12;

/// Cells the caret glyph itself and the gap beside it keep from the note.
///
/// Matches [`crate::widgets::sparkline::SparklineCaret`]'s own reservation —
/// "one cell for the caret, one for the gap, the rest for the note" — so
/// [`caret_note`]'s budget describes the same row the widget will place it in.
const CARET_NOTE_RESERVE: u16 = 2;

/// Borders for a panel whose bottom edge is the next panel's top rule.
///
/// §5.5 shares borders between vertically adjacent panels — one row is
/// simultaneously the bottom of the pressure radar and the top of the process
/// table — so a screen omits the duplicate side rather than drawing two rows of
/// `+---+` and spending a row of content on it.
pub(crate) const SHARED_BOTTOM: Borders = Borders::ALL.difference(Borders::BOTTOM);

/// Borders for the left-hand panel of a side-by-side pair.
///
/// Its right edge is the neighbour's left border, for the same reason.
pub(crate) const SHARED_RIGHT: Borders = Borders::ALL.difference(Borders::RIGHT);

/// Borders for the left-hand panel of a pair that also shares its bottom edge.
pub(crate) const SHARED_RIGHT_AND_BOTTOM: Borders = Borders::ALL
    .difference(Borders::RIGHT)
    .difference(Borders::BOTTOM);

/// Where the screen-independent parts of one frame go, and what is left over.
///
/// Pure geometry: resolving a `Chrome` draws nothing, so a screen can ask where
/// its body is without caring whether the chrome has been drawn yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Chrome {
    /// The panel geometry for this terminal size (§5.7).
    pub layout: Layout,
    /// The one-line CPU/memory/load strip, in the `Compact` band only (§5.7).
    ///
    /// `None` at every other breakpoint, where the header carries real meters and
    /// [`Layout::summary`] belongs to the screen instead.
    pub summary_strip: Option<Rect>,
    /// Everything between the chrome and the status footer: the screen's canvas.
    pub body: Option<Rect>,
}

impl Chrome {
    /// Resolves the chrome geometry for `area`.
    #[must_use]
    pub fn resolve(area: Rect) -> Self {
        let layout = Layout::resolve(area);
        // §5.7 gives `summary` two different jobs. In `Compact` it is the one-line
        // CPU/memory/load strip, which every screen needs because the condensed
        // header has no room for meters — so the chrome claims it. Everywhere else
        // it is "one lower summary panel selected by focus", which is the screen's
        // to fill.
        let summary_strip = match layout.breakpoint {
            Breakpoint::Compact => layout.summary,
            _ => None,
        };
        let body = body_area(&layout, summary_strip);
        Self {
            layout,
            summary_strip,
            body,
        }
    }

    /// Which §5.7 band this frame is in.
    #[must_use]
    pub const fn breakpoint(&self) -> Breakpoint {
        self.layout.breakpoint
    }
}

/// The rectangle below the chrome and above the status footer.
///
/// Derived from the panels the chrome occupies rather than from the breakpoint's
/// row constants, so it cannot drift out of step with [`Layout`]. Returns `None`
/// when nothing is left, which happens on a terminal too short for a body at all.
fn body_area(layout: &Layout, summary_strip: Option<Rect>) -> Option<Rect> {
    let area = layout.area;
    if area.is_empty() {
        return None;
    }
    let bottom_of = |rect: Rect| rect.y.saturating_add(rect.height);
    let top = [layout.header, summary_strip]
        .into_iter()
        .flatten()
        .map(bottom_of)
        .fold(area.y, u16::max);
    let bottom = layout
        .status
        .map_or_else(|| bottom_of(area), |status| status.y);
    let height = bottom.saturating_sub(top);
    let body = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height,
    };
    if body.is_empty() { None } else { Some(body) }
}

/// Draws one whole frame: the chrome, then the screen for [`AppState::view`].
///
/// The only entry point the runtime needs. `area` is the frame area; every
/// rectangle below it is derived, and a zero-area frame draws nothing rather
/// than panicking (§5.7).
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, presentation: Presentation<'_>) {
    let chrome = Chrome::resolve(area);

    if chrome.layout.shows_notice() {
        render_resize_notice(frame, area, state, presentation);
        return;
    }

    render_chrome(frame, area, state, presentation);

    // §5.7: below 80×20 but at least 60×16, the band renders "a stable minimal
    // process list". Stable means the same list whichever view is selected, so
    // the dispatch is deliberately skipped here rather than every screen having
    // to degrade into a process table of its own.
    if chrome.breakpoint() == Breakpoint::TooSmall {
        processes::render_minimal(frame, area, state, presentation);
        return;
    }

    match state.view() {
        ViewId::Overview => overview::render(frame, area, state, presentation),
        ViewId::Processes => processes::render(frame, area, state, presentation),
        ViewId::Cpu => cpu::render(frame, area, state, presentation),
        ViewId::Storage => storage::render(frame, area, state, presentation),
        ViewId::Network => network::render(frame, area, state, presentation),
        ViewId::Inspect => inspect::render(frame, area, state, presentation),
        ViewId::Battery => battery::render(frame, area, state, presentation),
    }
}

/// Draws the header, the compact summary strip, and the status footer.
///
/// Separate from [`render`] so a test — or a future screen that wants the chrome
/// without the dispatch — can draw exactly the frame furniture.
pub fn render_chrome(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    let chrome = Chrome::resolve(area);
    if let Some(header) = chrome.layout.header {
        render_header(frame, header, state, presentation);
    }
    if let Some(strip) = chrome.summary_strip {
        render_summary_strip(frame, strip, state, presentation);
    }
    if let Some(status) = chrome.layout.status {
        render_status_footer(frame, status, state, presentation);
    }
}

// ---------------------------------------------------------------------------
// Header (§5.5)
// ---------------------------------------------------------------------------

/// Draws §5.5's header: identity, timeline badge, interval, uptime, clock, and —
/// where the band has the rows for them — the CPU and memory meters.
///
/// The title line is written segment by segment rather than through
/// [`Panel::with_trailing`] because the timeline badge needs its own token and
/// symbol: §26 requires historical state to be visually unmistakable from live,
/// and a title rendered in one style cannot carry that.
pub fn render_header(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    let snapshot = state.snapshot().map(AsRef::as_ref);
    let buffer = frame.buffer_mut();

    // An empty title, so the panel draws the rule and the corners and leaves the
    // whole row free for the segments below.
    let panel = Panel::new(presentation, "")
        .with_borders(Borders::ALL.difference(Borders::BOTTOM))
        .focused(state.timeline_status().is_frozen());
    let inner = panel.inner(area);
    panel.render(area, buffer);

    {
        let mut painter = Painter::new(buffer, area);
        let width = painter.width();
        let clock = format!(" {} ", wall_clock(snapshot));
        let clock_width = u16::try_from(display_width(&clock)).unwrap_or(0);
        // The clock is reserved from the geometry rather than appended, so the
        // title can never be written across it (§5.4). One cell in from the left
        // corner, stopping one cell short of the clock's own left edge.
        let title_room = width
            .saturating_sub(RULE_TAIL)
            .saturating_sub(clock_width)
            .saturating_sub(2);
        let mut row = RowBuilder::new(title_room, presentation.glyphs());
        push_fitting(
            &mut row,
            presentation,
            &header_segments(state, snapshot, title_room),
        );
        let consumed = row.cursor();
        painter.write_line(1, 0, title_room, &row.finish());
        // One blank cell after whatever field the title ended on, so the last value
        // never abuts the horizontal rule. Written rather than reserved, because
        // which field ends the title depends on the width (§5.4's priority order).
        painter.write_within(
            1u16.saturating_add(consumed),
            0,
            1,
            " ",
            presentation.style(Token::Muted),
        );

        painter.write_right(
            0,
            0,
            width.saturating_sub(RULE_TAIL),
            &clock,
            presentation.style(Token::Muted),
        );
    }

    if inner.is_empty() {
        return;
    }
    // §5.5 starts the meters one cell in from the border.
    let inner = inset(inner);
    // §5.6: while a specific earlier sample is selected, the header meters describe
    // *that* sample. Mixing the selected sample's offset with live values in one row
    // would be exactly the confusion §26 forbids, so when a sample is selected both
    // the values and the notes come from it and nothing on the row is live.
    let selected = state
        .timeline()
        .is_historical()
        .then(|| state.timeline().selected_sample(state.history()))
        .flatten();
    let (cpu_value, memory_value, notes) = match selected {
        Some(sample) => (
            sample.system.cpu_busy,
            sample.system.memory_used_share,
            historical_notes(sample, state, presentation, inner.width),
        ),
        None => (
            snapshot.map_or(MetricState::WarmingUp, |snapshot| {
                snapshot.cpu.total.map(|usage| usage.busy)
            }),
            snapshot.map_or(MetricState::WarmingUp, |snapshot| snapshot.memory.usage),
            header_notes(snapshot, presentation, inner.width),
        ),
    };
    let meters = [
        ("CPU", cpu_value, notes.cpu.as_str()),
        ("MEM", memory_value, notes.memory.as_str()),
    ];
    for (index, (label, value, note)) in meters.into_iter().enumerate() {
        let Ok(offset) = u16::try_from(index) else {
            break;
        };
        if offset >= inner.height {
            break;
        }
        let row = Rect {
            y: inner.y.saturating_add(offset),
            height: 1,
            ..inner
        };
        Meter::new(presentation, value)
            .with_label(label)
            .with_label_width(HEADER_LABEL_WIDTH)
            .with_note(note)
            .with_note_width(notes.width)
            .render(row, buffer);
    }
}

/// The styled segments of the header's title line, left to right.
///
/// The host name is the only field that is *truncated* rather than dropped, and
/// the timeline badge is the only one that is never either. §2.1 requires the
/// header to display the timeline state, so the badge, the interval, and the lag
/// warning are reserved first and the host name takes what is left; the uptime is
/// appended only if it still fits.
fn header_segments(
    state: &AppState,
    snapshot: Option<&SystemSnapshot>,
    budget: u16,
) -> Vec<(String, Token)> {
    let status = state.timeline_status();
    let health = state.health();

    // The leading space separates the first field from the panel's corner, as
    // §5.5's `+ monitrs host:...` shows. Every later gap is [`push_fitting`]'s.
    let brand = " monitrs".to_owned();
    // The badge is bracketed so the state reads as a state and not as part of the
    // host name, and so its width changing cannot be mistaken for a value moving
    // (§5.4 governs values, not states).
    let badge = format!("[{}{}]", status.symbol(), status.label());
    let interval = format_duration(state.sample_interval());
    // §16.2 requires collector lag to be displayed. Reserved only while the
    // collector is behind: a healthy one must not spend header cells on a blank.
    let lag = if health.is_behind(state.sample_interval()) {
        format!("lag {}", format_duration(health.lag))
    } else {
        String::new()
    };

    // One gap per field that will be pushed after the brand.
    let gaps = SEGMENT_GAP * (3 + usize::from(!lag.is_empty()));
    let reserved = display_width(&brand)
        + display_width(&badge)
        + display_width(&interval)
        + display_width(&lag)
        + gaps;
    let host_room = usize::from(budget).saturating_sub(reserved);
    let hostname = snapshot.map_or("unknown", |snapshot| snapshot.host.display_hostname());
    let host = if host_room > HOST_PREFIX.len() {
        format!(
            "{HOST_PREFIX}{}",
            monitrs_core::units::truncate_tail(
                hostname,
                host_room - HOST_PREFIX.len(),
                budget_ellipsis(),
            )
        )
    } else {
        String::new()
    };

    let mut segments = vec![(brand, Token::Accent)];
    if !host.is_empty() {
        segments.push((host, Token::Text));
    }
    segments.push((badge, status.token()));
    segments.push((interval, Token::Muted));
    if !lag.is_empty() {
        segments.push((lag, Token::Watch));
    }
    let uptime = snapshot.map_or(MetricState::WarmingUp, |snapshot| snapshot.host.uptime);
    let uptime = states::describe(&uptime, |value| format_uptime(*value));
    segments.push((format!("up{}", uptime.flagged()), Token::Muted));
    segments
}

/// The `host:` prefix, kept as a constant so its width can be reserved.
const HOST_PREFIX: &str = "host:";

/// The truncation marker used when budgeting the host name.
///
/// Strict ASCII, deliberately: the host name is budgeted before the glyph mode is
/// consulted, and the ASCII marker is the wider of the two — so a name that fits
/// this budget fits in enhanced mode as well (§5.1).
const fn budget_ellipsis() -> monitrs_core::units::Ellipsis {
    monitrs_core::units::Ellipsis::Ascii
}

/// The notes beside the two header meters, and the width reserved for them.
struct HeaderNotes {
    cpu: String,
    memory: String,
    width: u16,
}

/// Builds the header meter notes, fitted to the room a `width`-cell header has.
///
/// The width is reserved from geometry and shared by both meters, so a value
/// crossing a unit boundary cannot lengthen its note and shorten its bar (§5.4).
/// The note *contents* are assembled greedily from a priority list, so a narrow
/// terminal loses the battery before it loses the load average.
fn header_notes(
    snapshot: Option<&SystemSnapshot>,
    presentation: Presentation<'_>,
    width: u16,
) -> HeaderNotes {
    let budget = usize::from(width.saturating_sub(HEADER_NOTE_RESERVE));
    let units = presentation.units();

    let Some(snapshot) = snapshot else {
        return HeaderNotes {
            cpu: String::new(),
            memory: String::new(),
            width: 0,
        };
    };

    let cpu = join_fitting(&cpu_note_segments(snapshot), budget);
    let memory = join_fitting(&memory_note_segments(snapshot, units), budget);
    let used = display_width(&cpu).max(display_width(&memory));
    HeaderNotes {
        cpu,
        memory,
        width: u16::try_from(used).unwrap_or(u16::MAX),
    }
}

/// The header notes for a selected historical sample (§2.1, §5.6).
///
/// Everything here comes from the retained sample, never from the live snapshot: a
/// row that mixed the two would be a frame that is partly now and partly not, which
/// is the failure §26 names.
fn historical_notes(
    sample: &HistoricalSample,
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> HeaderNotes {
    let budget = usize::from(width.saturating_sub(HEADER_NOTE_RESERVE));
    let units = presentation.units();
    let load = states::describe(&sample.system.load_one, |value| format!("{value:.2}"));
    let swap = states::describe_bytes(&sample.system.swap_used_bytes, units);
    let behind = state.timeline().view().offset_from_live(state.history());

    let cpu = join_fitting(
        &[
            format!("load1 {}", load.flagged()),
            format!("sample {}", wall_clock_of(sample.wall_time)),
        ],
        budget,
    );
    let memory = join_fitting(
        &[
            format!("-{} behind live", format_age(behind)),
            format!("swap {}", swap.flagged().trim_start()),
        ],
        budget,
    );
    let used = display_width(&cpu).max(display_width(&memory));
    HeaderNotes {
        cpu,
        memory,
        width: u16::try_from(used).unwrap_or(u16::MAX),
    }
}

/// The CPU meter's note segments, highest priority first (§7.1: load, cores).
fn cpu_note_segments(snapshot: &SystemSnapshot) -> Vec<String> {
    let mut segments = vec![format!("load {}", load_display(&snapshot.load).flagged())];
    segments.push(format!("{} cpu", snapshot.cpu.logical_count));
    if let Some(&physical) = snapshot.cpu.physical_count.fresh() {
        segments.push(format!("{physical} core"));
    }
    // Directly after the counts it qualifies, and ahead of the temperature: when the
    // header runs out of room the segment a reader can least afford to lose is the one
    // saying those counts are not the ones that apply here (§9.2).
    if let Some(text) = cgroup_cpu_text(snapshot) {
        segments.push(text);
    }
    if let Some(display) = temperature_display(&snapshot.sensors) {
        segments.push(display.annotated());
    }
    segments
}

/// The memory meter's note segments, highest priority first (§7.1, §9.2).
fn memory_note_segments(snapshot: &SystemSnapshot, units: ByteUnits) -> Vec<String> {
    let mut segments = vec![
        used_of_total_text(snapshot, units),
        swap_text(snapshot, units),
    ];
    if let Some(text) = cgroup_limit_text(snapshot, units) {
        segments.push(text);
    }
    if let Some((battery, _)) = snapshot.sensors.battery.displayable() {
        segments.push(battery_text(battery));
    }
    segments
}

/// `23G/32G` — memory in use against the machine total.
fn used_of_total_text(snapshot: &SystemSnapshot, units: ByteUnits) -> String {
    let memory = &snapshot.memory;
    let used = states::describe_bytes(&memory.used, units);
    format!(
        "{}/{}",
        used.flagged().trim_start(),
        format_bytes_compact(memory.total_bytes, units)
    )
}

/// `cgroup 2.0G`, or `None` when this process tree has the whole machine.
///
/// §9.2: a container's limit is shown *beside* the host total, never folded into it.
fn cgroup_limit_text(snapshot: &SystemSnapshot, units: ByteUnits) -> Option<String> {
    let memory = &snapshot.memory;
    let limit = memory.effective_limit_bytes();
    (limit != memory.total_bytes).then(|| format!("cgroup {}", format_bytes_compact(limit, units)))
}

/// `cgroup 1.5 cpu`, or `None` when this process tree has every CPU the machine has.
///
/// The CPU counterpart of [`cgroup_limit_text`], and here for the same §9.2 reason: the
/// header states what the machine has, and inside a container that is not what the
/// processes in it may use.
fn cgroup_cpu_text(snapshot: &SystemSnapshot) -> Option<String> {
    snapshot
        .cpu
        .is_cpu_limited()
        .then(|| format!("cgroup {:.1} cpu", snapshot.cpu.effective_cores()))
}

/// `swap 205M/2.0G`, or `swap off` when swap is not configured.
///
/// "off" rather than a placeholder: [`SwapSnapshot::total_bytes`] of zero is a
/// fact about the machine, not a metric the OS withheld.
///
/// [`SwapSnapshot::total_bytes`]: monitrs_core::model::SwapSnapshot::total_bytes
fn swap_text(snapshot: &SystemSnapshot, units: ByteUnits) -> String {
    let swap = &snapshot.memory.swap;
    if !swap.is_enabled() {
        return "swap off".to_owned();
    }
    let used = states::describe_bytes(&swap.used, units);
    format!(
        "swap {}/{}",
        used.flagged().trim_start(),
        format_bytes_compact(swap.total_bytes, units)
    )
}

/// `62.5C` for the hottest sensor, with `!` when the sensor calls it critical.
///
/// The threshold comes from the sensor itself; §11.3 forbids diagnosing thermal
/// throttling, so nothing here draws a conclusion beyond what the hardware
/// declares.
fn temperature_text(reading: &TemperatureReading) -> String {
    let flag = if reading.is_critical() == Some(true) {
        '!'
    } else {
        ' '
    };
    format!("temp{flag}{:.1}C", reading.celsius)
}

/// The hottest sensor reading as a display, so a retained one carries its age.
///
/// Goes through the metric's own state rather than through
/// [`SensorSnapshot::hottest`], which filters to freshly measured lists: since
/// sensors moved to their own cadence a reading can legitimately be 30 seconds
/// old, and a header that dropped it would be less informative than one that
/// dates it (§4, and the design document's A2).
fn temperature_display(sensors: &SensorSnapshot) -> Option<MetricDisplay> {
    let has_reading = sensors
        .temperatures
        .displayable()
        .is_some_and(|(readings, _)| !readings.is_empty());
    if !has_reading {
        return None;
    }
    Some(states::describe(&sensors.temperatures, |readings| {
        readings
            .iter()
            .max_by(|left, right| left.celsius.total_cmp(&right.celsius))
            .map_or_else(String::new, temperature_text)
    }))
}

/// `bat 82%-`, where the trailing character is the charge state's own cue (§5.2).
fn battery_text(battery: &BatterySnapshot) -> String {
    format!("bat {}{}", battery.charge, battery.state.symbol())
}

/// The load average as one display, so its availability travels with its text.
fn load_display(load: &MetricState<LoadSnapshot>) -> MetricDisplay {
    states::describe(load, |value| {
        format!("{:.2} {:.2} {:.2}", value.one, value.five, value.fifteen)
    })
}

/// The header clock, as UTC `HH:MM:SSZ` taken from the snapshot's wall time.
///
/// Two deliberate decisions.
///
/// * The time comes from [`SystemSnapshot::wall_time`], never from a clock read:
///   the renderer performs no I/O and no state-affecting clock read (§10.5), and
///   a frame that read the clock itself could not be snapshot-tested.
/// * It is UTC, and says so. §13's dependency policy keeps a time-zone database
///   out of the binary, so the local offset is not knowable here; printing a bare
///   `22:14:44` that was really UTC would be a lie, and one cell of `Z` is the
///   whole cost of not telling it.
fn wall_clock(snapshot: Option<&SystemSnapshot>) -> String {
    let Some(snapshot) = snapshot else {
        return "--:--:--Z".to_owned();
    };
    let Ok(since_epoch) = snapshot.wall_time.duration_since(SystemTime::UNIX_EPOCH) else {
        // A wall clock set before 1970. Ordering never depends on it (§8.1), so
        // this is a display curiosity rather than a failure.
        return "--:--:--Z".to_owned();
    };
    let seconds = since_epoch.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

// ---------------------------------------------------------------------------
// The Compact summary strip (§5.7)
// ---------------------------------------------------------------------------

/// Draws §5.7's one-line CPU/memory/load summary for the `Compact` band.
///
/// The band's header has no room for meters, so this row is the only place the
/// aggregate numbers appear. Every field carries its availability symbol, so the
/// line is still readable with colour off (§5.2).
pub fn render_summary_strip(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    let snapshot = state.snapshot().map(AsRef::as_ref);
    let buffer = frame.buffer_mut();
    let mut painter = Painter::new(buffer, area);
    let width = painter.width();
    if width == 0 {
        return;
    }
    let mut row = RowBuilder::new(width, presentation.glyphs());
    // One cell in, so the strip lines up with the panel contents above and below it.
    row.pad(1);
    push_fitting(
        &mut row,
        presentation,
        &summary_segments(snapshot, presentation),
    );
    let line = row.finish();
    painter.write_line(0, 0, width, &line);
}

/// The summary strip's fields, highest priority first.
///
/// The order is §5.7's own — "CPU/memory/load summary" first — then the figures
/// §7.1 asks the Overview for, and the optional sensor summary last because it is
/// what a narrow terminal can most afford to lose.
fn summary_segments(
    snapshot: Option<&SystemSnapshot>,
    presentation: Presentation<'_>,
) -> Vec<(String, Token)> {
    let units = presentation.units();
    let Some(snapshot) = snapshot else {
        return vec![("no sample yet".to_owned(), Token::Muted)];
    };
    let cpu = states::describe_percent(&snapshot.cpu.total.map(|usage| usage.busy));
    let memory = states::describe_percent(&snapshot.memory.usage);
    let (read, write) = aggregate_disk_rates(snapshot);
    let (rx, tx) = aggregate_network_rates(snapshot);
    let rate = |state| {
        states::describe_byte_rate(&state, units)
            .flagged()
            .trim()
            .to_owned()
    };

    let mut segments = vec![
        (format!("CPU{}", cpu.flagged()), cpu.token()),
        (format!("MEM{}", memory.flagged()), memory.token()),
        (
            format!("load {}", load_display(&snapshot.load).flagged()),
            Token::Muted,
        ),
        (used_of_total_text(snapshot, units), Token::Muted),
        (swap_text(snapshot, units), Token::Muted),
        (
            format!("dsk r{} w{}", rate(read), rate(write)),
            Token::Muted,
        ),
        (format!("net rx{} tx{}", rate(rx), rate(tx)), Token::Muted),
    ];
    if let Some(text) = cgroup_limit_text(snapshot, units) {
        segments.push((text, Token::Muted));
    }
    if let Some(display) = temperature_display(&snapshot.sensors) {
        segments.push((display.annotated(), Token::Muted));
    }
    if let Some((battery, _)) = snapshot.sensors.battery.displayable() {
        segments.push((battery_text(battery), Token::Muted));
    }
    segments
}

// ---------------------------------------------------------------------------
// Status footer (§5.5)
// ---------------------------------------------------------------------------

/// Draws §5.5's status footer: the tab strip, the filter state, the newest
/// notice, and the key hints.
///
/// The tab strip marks the active view with brackets, which is a non-colour cue
/// and — because the brackets replace the padding spaces rather than adding to
/// them — costs no width, so switching view never shifts the strip (§5.2, §5.4).
pub fn render_status_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    let buffer = frame.buffer_mut();
    let mut painter = Painter::new(buffer, area);
    let width = painter.width();
    if width == 0 {
        return;
    }

    let hints = join_fitting(&hint_segments(state), usize::from(width));
    let hint_width = u16::try_from(display_width(&hints)).unwrap_or(0);
    let left_room = width.saturating_sub(hint_width);

    let mut row = RowBuilder::new(left_room, presentation.glyphs());
    for (text, token) in tab_segments(state, left_room) {
        if row.is_full() {
            break;
        }
        row.push(&text, presentation.style(token));
    }
    if state.filter().is_active() {
        let label = format!("  filter:{}", state.filter_text());
        row.push(&label, presentation.style(Token::Accent));
    }
    let consumed = row.cursor();
    painter.write_line(0, 0, left_room, &row.finish());

    // The newest notice goes in whatever gap is left between the tabs and the
    // hints. §14.1 keeps notices off stdout while the alternate screen is up, and
    // the Inspect screen renders the whole log — this is the one-line summary.
    if let Some(notice) = state.notice_log().most_severe() {
        let gap = left_room.saturating_sub(consumed);
        if gap >= MIN_NOTICE_WIDTH {
            let text = format!("  {}", notice.render());
            painter.write_within(
                consumed,
                0,
                gap,
                &states::fit_within(&text, usize::from(gap), presentation.glyphs()),
                presentation.style(notice.token()),
            );
        }
    }

    painter.write_right(0, 0, width, &hints, presentation.style(Token::Muted));
}

/// The tab strip's segments, condensed to bare digits when the titles do not fit.
fn tab_segments(state: &AppState, room: u16) -> Vec<(String, Token)> {
    let full: usize = ViewId::ALL
        .iter()
        .map(|view| display_width(view.title()) + 4)
        .sum();
    let titled = full <= usize::from(room);
    ViewId::ALL
        .into_iter()
        .map(|view| {
            let active = view == state.view();
            let (open, close) = if active { ('[', ']') } else { (' ', ' ') };
            let body = if titled {
                format!("{} {}", view.digit(), view.title())
            } else {
                view.digit().to_string()
            };
            let token = if active { Token::Accent } else { Token::Muted };
            (format!("{open}{body}{close}"), token)
        })
        .collect()
}

/// The footer's key hints, highest priority first.
///
/// The *keys* come from the active keymap so a rebind is reflected (§7.6's rule
/// that nothing duplicates the keymap), while the words are §5.5's own —
/// `filter`, `help` — because a footer has no room for the keymap's full
/// sentences and the help overlay is where those belong.
fn hint_segments(state: &AppState) -> Vec<String> {
    let mode = state.input_mode();
    let mut hints = Vec::new();
    // Returning to live is the one explicit action §2.1 requires, so it is the
    // first hint whenever the view is not live.
    if state.timeline_status().is_frozen()
        && let Some(key) = binding_key(state, mode, &Action::ReturnLive)
    {
        hints.push(format!("{key} live"));
    }
    if let Some(key) = binding_key(state, mode, &Action::BeginFilterEdit) {
        hints.push(format!("{key} filter"));
    }
    if let Some(key) = binding_key(state, mode, &Action::ToggleHelp) {
        hints.push(format!("{key} help"));
    }
    hints
}

/// The label of the first key bound to `action` in `mode`, if there is one.
fn binding_key(
    state: &AppState,
    mode: crate::keymap::InputMode,
    action: &Action,
) -> Option<String> {
    state
        .keymap()
        .bindings_for_mode(mode)
        .find(|binding| binding.action() == Some(action))
        .map(|binding| binding.chord.label())
}

// ---------------------------------------------------------------------------
// The resize notice (§5.7)
// ---------------------------------------------------------------------------

/// Draws §5.7's three-line resize notice, centred in the frame.
///
/// The text is [`unusable_notice`]'s, verbatim, because it is user-visible and
/// pinned by a snapshot test.
pub fn render_resize_notice(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    let (columns, rows) = state.size();
    let lines = unusable_notice(columns, rows);
    let buffer = frame.buffer_mut();
    let mut painter = Painter::new(buffer, area);
    let width = painter.width();
    let height = painter.height();
    if width == 0 || height == 0 {
        return;
    }
    let top = height.saturating_sub(u16::try_from(lines.len()).unwrap_or(0)) / 2;
    for (index, text) in lines.iter().enumerate() {
        let Ok(offset) = u16::try_from(index) else {
            break;
        };
        let y = top.saturating_add(offset);
        if y >= height {
            break;
        }
        let token = if index == 0 {
            Token::Text
        } else {
            Token::Muted
        };
        let text_width = u16::try_from(display_width(text)).unwrap_or(width);
        let x = width.saturating_sub(text_width) / 2;
        painter.write(x, y, text, presentation.style(token));
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Draws a bordered panel and returns the rectangle inside it.
///
/// The one place in this module a panel is constructed, so the §5.5 conventions —
/// the title, the right-aligned trailing label, the focus border, and the shared
/// edges between adjacent panels — cannot be applied inconsistently between
/// screens.
pub(crate) fn draw_bordered_panel(
    buffer: &mut Buffer,
    area: Rect,
    presentation: Presentation<'_>,
    title: &str,
    trailing: Option<&str>,
    focused: bool,
    borders: Borders,
) -> Rect {
    let mut panel = Panel::new(presentation, title)
        .focused(focused)
        .with_borders(borders);
    if let Some(trailing) = trailing {
        panel = panel.with_trailing(trailing);
    }
    let inner = panel.inner(area);
    panel.render(area, buffer);
    inner
}

/// As many of `parts` as fit beside `title` in a panel's trailing label, in order.
///
/// [`Panel`] drops its trailing label *whole* rather than truncating it, which is right
/// for a count and wrong for a sentence: at 80 columns the full text does not fit, and
/// without this the parts that *would* have fitted vanish along with the one that
/// overflowed. So they are fitted here in priority order, and a part that does not fit
/// ends the label rather than letting a shorter one behind it jump the queue (§5.4).
///
/// Deliberately pessimistic about the chrome: a part dropped one cell early costs a
/// clause, while one kept a cell too late costs the whole label.
pub(crate) fn fit_label(parts: &[String], title: &str, panel_width: u16) -> String {
    // The corners, the rule either side of the title, and the three cells the panel
    // keeps before its right corner.
    let chrome = display_width(title) + 10;
    let room = usize::from(panel_width).saturating_sub(chrome);
    let mut out = String::new();
    for part in parts {
        let separator = if out.is_empty() { 0 } else { 2 };
        if display_width(&out) + separator + display_width(part) > room {
            break;
        }
        if separator > 0 {
            out.push_str(", ");
        }
        out.push_str(part);
    }
    out
}

/// The rectangle a panel with `borders` would leave inside `area`, without drawing.
///
/// Screens need the interior height *before* the panel is drawn, because the
/// trailing label has to state the truncation the interior causes. Delegated to
/// [`Panel::inner`] rather than reimplemented, so the two can never disagree.
pub(crate) fn inner_of(presentation: Presentation<'_>, area: Rect, borders: Borders) -> Rect {
    Panel::new(presentation, "")
        .with_borders(borders)
        .inner(area)
}

/// One cell trimmed from the left of a panel interior.
///
/// §5.5's meter, radar, history, and pin rows all begin one cell in from the
/// border; only the tables sit flush against it, because their first column is a
/// one-cell marker and an inset would put the marker under the frame. Applying it
/// here rather than at each call site is what keeps the two conventions from
/// drifting into three.
pub(crate) fn inset(area: Rect) -> Rect {
    if area.width < 2 {
        return area;
    }
    Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(1),
        ..area
    }
}

/// Pushes styled segments while each one fits whole, stopping at the first that
/// does not.
///
/// Tail-truncating the field that ran out of room would leave a fragment reading
/// as data — `ds...` where `dsk r18M/s w42M/s` belonged — so a field either
/// appears in full or not at all, and the order is the priority (§5.4). The gap is
/// [`SEGMENT_GAP`], the same one [`join_fitting`] uses and the one §5.5's header
/// shows between `host:dev-mbp` and `LIVE`.
pub(crate) fn push_fitting(
    row: &mut RowBuilder,
    presentation: Presentation<'_>,
    segments: &[(String, Token)],
) {
    let gap = u16::try_from(SEGMENT_GAP).unwrap_or(1);
    for (index, (text, token)) in segments.iter().enumerate() {
        let gap = if index == 0 { 0 } else { gap };
        let needed = u16::try_from(display_width(text)).unwrap_or(u16::MAX);
        if row.remaining() < needed.saturating_add(gap) {
            break;
        }
        row.pad(gap);
        row.push(text, presentation.style(*token));
    }
}

/// A single muted line of `text`, for the "nothing here, and here is why" rows.
///
/// Never used for a metric: those go through [`crate::widgets::states`] so §4's
/// placeholder, token, and symbol are decided in one place. This is for the
/// sentences a *panel* says about itself — "no sample yet", "not mapped".
pub(crate) fn muted_line(presentation: Presentation<'_>, width: u16, text: &str) -> Line<'static> {
    let mut row = RowBuilder::new(width, presentation.glyphs());
    row.push(text, presentation.style(Token::Muted));
    row.finish()
}

/// Writes `lines` down `area`, one per row, clipped to the rectangle.
pub(crate) fn write_lines(buffer: &mut Buffer, area: Rect, lines: &[Line<'static>]) {
    let mut painter = Painter::new(buffer, area);
    let width = painter.width();
    let height = painter.height();
    for (index, line) in lines.iter().enumerate() {
        let Ok(y) = u16::try_from(index) else { break };
        if y >= height {
            break;
        }
        painter.write_line(0, y, width, line);
    }
}

/// Splits `area` into stacked rows of the given heights, top to bottom.
///
/// A row that does not fit is returned empty rather than omitted, so callers can
/// index positionally; an empty rectangle renders nothing (§5.7).
pub(crate) fn split_rows(area: Rect, heights: &[u16]) -> Vec<Rect> {
    let mut out = Vec::with_capacity(heights.len());
    let mut y = area.y;
    let bottom = area.y.saturating_add(area.height);
    for height in heights {
        let available = bottom.saturating_sub(y);
        let taken = (*height).min(available);
        out.push(Rect {
            x: area.x,
            y,
            width: area.width,
            height: taken,
        });
        y = y.saturating_add(taken);
    }
    out
}

/// Splits `area` into two columns, the left one `left` cells wide.
pub(crate) fn split_columns(area: Rect, left: u16) -> (Rect, Rect) {
    let taken = left.min(area.width);
    (
        Rect {
            width: taken,
            ..area
        },
        Rect {
            x: area.x.saturating_add(taken),
            width: area.width.saturating_sub(taken),
            ..area
        },
    )
}

/// How many rows to skip so `selected` is visible in a `visible`-row window.
///
/// Deliberately stateless. A scroll offset is display state, and [`AppState`] is
/// frozen: adding a field to it is not this module's to do, and deriving the
/// offset keeps the renderer a pure function of the state anyway. The rule is:
/// while the selection is inside the first window nothing scrolls, and beyond it
/// the selection sits on the last visible row. Predictable in both directions,
/// and it never hides the cursor — which is what §7.2's "do not allow row
/// selection to jump unpredictably" is protecting.
pub(crate) fn scroll_offset(selected: Option<usize>, visible: usize, total: usize) -> usize {
    let Some(selected) = selected else { return 0 };
    if visible == 0 {
        return 0;
    }
    let max_scroll = total.saturating_sub(visible);
    selected
        .saturating_sub(visible.saturating_sub(1))
        .min(max_scroll)
}

/// Joins `segments` with two spaces while the total stays inside `budget`.
///
/// Segments are considered in order and a segment that does not fit ends the
/// line, rather than being skipped in favour of a shorter one behind it: the
/// order *is* the priority, and a strip whose fields reordered themselves as
/// values changed would be unreadable (§5.4).
pub(crate) fn join_fitting(segments: &[String], budget: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for segment in segments {
        let width = display_width(segment);
        let gap = if out.is_empty() { 0 } else { SEGMENT_GAP };
        if used + gap + width > budget {
            break;
        }
        for _ in 0..gap {
            out.push(' ');
        }
        out.push_str(segment);
        used += gap + width;
    }
    out
}

/// A row builder for `width` cells in `presentation`'s glyph mode.
pub(crate) fn row_builder(presentation: Presentation<'_>, width: u16) -> RowBuilder {
    RowBuilder::new(width, presentation.glyphs())
}

/// The trailing label for a panel whose content did not fit, or `None`.
///
/// §2.3 and §2.5 both forbid silent truncation of a panel that promises to show
/// everything, so a screen that drops rows says how many it dropped.
pub(crate) fn truncation_label(shown: usize, total: usize) -> Option<String> {
    (shown < total).then(|| format!("{shown} of {total}"))
}

// ---------------------------------------------------------------------------
// Aggregates and history series
// ---------------------------------------------------------------------------

/// The summed read and write throughput of every block device that reported one.
///
/// Delegated to [`monitrs_core::history::HistoricalSystemMetrics::from_snapshot`]
/// so the live aggregate and the retained one are the same arithmetic — including
/// the part that matters, which is that a group with nothing measured stays
/// unavailable instead of summing to zero (§4).
pub(crate) fn aggregate_disk_rates(
    snapshot: &SystemSnapshot,
) -> (MetricState<Rate>, MetricState<Rate>) {
    let metrics = monitrs_core::history::HistoricalSystemMetrics::from_snapshot(snapshot);
    (metrics.disk_read, metrics.disk_write)
}

/// The summed receive and transmit throughput of every non-loopback interface.
pub(crate) fn aggregate_network_rates(
    snapshot: &SystemSnapshot,
) -> (MetricState<Rate>, MetricState<Rate>) {
    let metrics = monitrs_core::history::HistoricalSystemMetrics::from_snapshot(snapshot);
    (metrics.network_rx, metrics.network_tx)
}

/// One retained aggregate as a plottable series, oldest sample first.
///
/// [`crate::widgets::Sparkline`] plots `MetricState<Percent>`, which is exactly
/// right for the two percentage metrics and a deliberate reinterpretation for the
/// rest: a self-scaling sparkline reads only the *magnitude* of each sample, so a
/// byte rate travels through the same channel as a bare number. The number is
/// never rendered as a percentage — [`plot_peak`] labels the scale instead, which
/// is the labelling a self-scaling plot owes the reader.
///
/// Availability is preserved sample by sample, so a counter reset stays a gap in
/// the plot rather than becoming a trough (§4, §21 M4).
pub(crate) fn plot_series(ring: &HistoryRing, metric: HistoryMetric) -> Vec<MetricState<Percent>> {
    ring.samples()
        .map(|sample| plot_sample(sample.system.measurement(metric)))
        .collect()
}

/// Converts one retained measurement into the value a sparkline can plot.
fn plot_sample(measurement: MetricState<MeasuredValue>) -> MetricState<Percent> {
    match measurement {
        MetricState::Available(value) => wrap_magnitude(value),
        MetricState::Stale { value, age } => match wrap_magnitude(value) {
            MetricState::Available(percent) => MetricState::Stale {
                value: percent,
                age,
            },
            other => other,
        },
        MetricState::WarmingUp => MetricState::WarmingUp,
        MetricState::PermissionDenied => MetricState::PermissionDenied,
        MetricState::Unsupported => MetricState::Unsupported,
        MetricState::TemporarilyUnavailable(reason) => MetricState::TemporarilyUnavailable(reason),
    }
}

/// The magnitude of a measurement as a plottable number.
fn wrap_magnitude(value: MeasuredValue) -> MetricState<Percent> {
    // Narrowing to f32 is what `Percent` stores. A magnitude too large to
    // represent becomes infinite, which `Percent::new` rejects — so the
    // conversion can lose precision but can never fabricate a value (§4).
    #[allow(clippy::cast_possible_truncation)]
    let magnitude = match value {
        MeasuredValue::Percent(percent) => percent.value(),
        MeasuredValue::Load(load) => load,
        MeasuredValue::Bytes(bytes) | MeasuredValue::Count(bytes) => bytes as f32,
        MeasuredValue::ByteRate(rate) | MeasuredValue::EventRate(rate) => rate.per_second() as f32,
        MeasuredValue::Duration(duration) => duration.as_secs_f32(),
    };
    Percent::new(magnitude).map_or(
        MetricState::TemporarilyUnavailable(monitrs_core::model::UnavailableReason::ParseFailed),
        MetricState::Available,
    )
}

/// The largest retained value of `metric`, rendered in its own units.
///
/// This is the scale label a self-scaling plot needs; without it the reader
/// cannot tell a 60 MiB/s peak from a 60 KiB/s one.
pub(crate) fn plot_peak(
    ring: &HistoryRing,
    metric: HistoryMetric,
    units: ByteUnits,
) -> Option<String> {
    let mut best: Option<(f64, MeasuredValue)> = None;
    for sample in ring.samples() {
        let Some(scalar) = sample.system.scalar(metric) else {
            continue;
        };
        let Some(&value) = sample.system.measurement(metric).fresh() else {
            continue;
        };
        if best.is_none_or(|(previous, _)| scalar > previous) {
            best = Some((scalar, value));
        }
    }
    best.map(|(_, value)| value.render(units))
}

/// The span of retained history, as the `HISTORY 5m` label of §5.5.
pub(crate) fn history_span_label(ring: &HistoryRing) -> String {
    format_duration(round_to_seconds(ring.limits().effective_duration()))
}

/// Rounds a duration down to whole seconds so the label reads `5m`, not `5m1ms`.
fn round_to_seconds(duration: Duration) -> Duration {
    Duration::from_secs(duration.as_secs())
}

/// How many samples back from the newest the Time Lens cursor is parked, if it is.
///
/// `None` while live: there is no caret to draw, and §2.1 requires the live view
/// to look live.
pub(crate) fn selected_sample_offset(state: &AppState) -> Option<usize> {
    if state.timeline_status().is_live() {
        return None;
    }
    let ring = state.history();
    let newest = ring.newest_absolute()?;
    let selected = state.timeline().view().selected_absolute(ring)?;
    usize::try_from(newest.saturating_sub(selected)).ok()
}

/// The caret's note: what the selected sample means, in priority order —
/// `22:14:07Z  cpu prev +41 points  30s +54 points  -00:37 selected` (§2.5),
/// or as much of that as `width` has room for.
///
/// This used to be a fixed `-00:37 selected 22:14:07Z` — *when* the selection
/// was, not what it means. §2.5 asks for the selected sample to be compared
/// against the previous one and against roughly 30 seconds ago, and this is
/// where that reaches the interface: [`HistoryView::comparisons`] has existed
/// since 0.1.0 with nothing calling it.
///
/// Built with [`join_fitting`] rather than a fixed format string, from four
/// segments taken in this order until the row runs out of room:
///
/// 1. **The sample's wall clock**, `22:14:07Z`. This is the segment worth
///    protecting most. At the `Standard` and `Wide` breakpoints it is *also*
///    shown in [`historical_notes`]'s header meter rows (`sample 22:14:07Z`),
///    so losing it here would cost nothing there — but at `Compact` (80-99
///    columns) the header collapses to one row with no space for
///    `historical_notes` (`Chrome::resolve`, `Layout::fill_compact`), and the
///    one-line strip it draws instead reads the *live* snapshot, never the
///    selection (§7.1's `render_summary_strip`). Below `Compact` this caret
///    note is the only place the selected sample's wall clock is shown at
///    all, which is why it now leads rather than trails.
/// 2. **`cpu prev …`**, the previous-sample comparison — §2.5's nearer
///    baseline, and the more actionable one.
/// 3. **`30s …`**, the thirty-second comparison — §2.5's other baseline.
/// 4. **`-00:37 selected`**, the relative offset. Lowest priority on purpose:
///    it is the one segment that is genuinely redundant everywhere, since the
///    header's `[<HISTORY -MM:SS]` badge carries the same figure at every
///    breakpoint including `Compact` (§2.1), and `historical_notes` repeats it
///    again (`-00:08 behind live`) wherever that panel has room.
pub(crate) fn caret_note(state: &AppState, units: ByteUnits, width: u16) -> String {
    let ring = state.history();
    let view = state.timeline().view();
    let offset = view.offset_from_live(ring);
    let sample = state.timeline().selected_sample(ring);
    let comparisons = view.comparisons(ring, HistoryMetric::CpuBusy);

    let segments: Vec<String> = [
        sample.map(|sample| wall_clock_of(sample.wall_time)),
        Some(format!(
            "cpu prev {}",
            baseline_delta(comparisons.previous_sample.as_ref(), units)
        )),
        Some(format!(
            "30s {}",
            baseline_delta(comparisons.thirty_seconds_ago.as_ref(), units)
        )),
        Some(format!("-{} selected", format_age(offset))),
    ]
    .into_iter()
    .flatten()
    .collect();

    let budget = usize::from(width.saturating_sub(CARET_NOTE_RESERVE));
    join_fitting(&segments, budget)
}

/// One baseline's rendered delta, or the word that replaces a missing one.
///
/// A baseline history cannot reach is `no baseline`, never `+0`: §2.5 asks for a
/// comparison "when history permits", and a zero delta would say the metric did
/// not change when the truth is that nothing was there to compare with (§26).
fn baseline_delta(comparison: Option<&MetricComparison>, units: ByteUnits) -> String {
    match comparison {
        Some(comparison) => comparison.render_delta(units),
        None => "no baseline".to_owned(),
    }
}

/// [`wall_clock`] for a bare [`SystemTime`], for the caret and sample labels.
pub(crate) fn wall_clock_of(time: SystemTime) -> String {
    let Ok(since_epoch) = time.duration_since(SystemTime::UNIX_EPOCH) else {
        return "--:--:--Z".to_owned();
    };
    let seconds = since_epoch.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}Z",
        seconds / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::model::CollectorHealth;

    use super::*;
    use crate::app::{AppSettings, AppState};

    fn state_of(width: u16, height: u16) -> AppState {
        AppState::new(AppSettings {
            size: (width, height),
            ..AppSettings::default()
        })
    }

    #[test]
    fn the_chrome_leaves_a_body_at_every_usable_breakpoint() {
        for (width, height) in [(140u16, 38u16), (100, 28), (80, 24), (60, 16)] {
            let chrome = Chrome::resolve(Rect::new(0, 0, width, height));
            let body = chrome.body.expect("a usable terminal has a body");
            assert!(!body.is_empty(), "{width}x{height}");
            let header = chrome.layout.header.expect("a header");
            assert!(
                body.y >= header.y + header.height,
                "the body must start below the header at {width}x{height}"
            );
            if let Some(status) = chrome.layout.status {
                assert!(
                    body.y + body.height <= status.y,
                    "the body must stop above the status line at {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn only_the_compact_band_claims_the_summary_slot_for_the_chrome() {
        // §5.7 gives `summary` two jobs; the chrome takes only the one-line form.
        assert!(
            Chrome::resolve(Rect::new(0, 0, 80, 24))
                .summary_strip
                .is_some()
        );
        assert!(
            Chrome::resolve(Rect::new(0, 0, 100, 28))
                .summary_strip
                .is_none()
        );
        assert!(
            Chrome::resolve(Rect::new(0, 0, 140, 38))
                .summary_strip
                .is_none()
        );
    }

    #[test]
    fn a_zero_area_frame_resolves_to_no_chrome_and_no_body() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 200, 0),
            Rect::new(0, 0, 0, 40),
        ] {
            let chrome = Chrome::resolve(area);
            assert!(chrome.body.is_none());
            assert!(chrome.summary_strip.is_none());
        }
    }

    #[test]
    fn the_wall_clock_reads_utc_and_says_so() {
        let midnight = SystemTime::UNIX_EPOCH;
        let snapshot = SystemSnapshot::warming_up(
            std::time::Instant::now(),
            midnight + Duration::from_secs(22 * 3_600 + 14 * 60 + 44),
            8,
        );
        assert_eq!(wall_clock(Some(&snapshot)), "22:14:44Z");
        assert_eq!(wall_clock(None), "--:--:--Z");
        // The suffix is what keeps the figure honest: no time-zone database is
        // linked, so a bare local-looking time would be a claim we cannot make.
        assert!(wall_clock(Some(&snapshot)).ends_with('Z'));
    }

    #[test]
    fn the_wall_clock_survives_a_pre_epoch_system_clock() {
        let snapshot = SystemSnapshot::warming_up(
            std::time::Instant::now(),
            SystemTime::UNIX_EPOCH - Duration::from_secs(60),
            8,
        );
        assert_eq!(wall_clock(Some(&snapshot)), "--:--:--Z");
    }

    #[test]
    fn joining_fields_keeps_the_priority_order_and_the_budget() {
        let segments = vec![
            "load 4.12".to_owned(),
            "8 cpu".to_owned(),
            "temp 62C".to_owned(),
        ];
        assert_eq!(join_fitting(&segments, 0), "");
        assert_eq!(join_fitting(&segments, 9), "load 4.12");
        assert_eq!(join_fitting(&segments, 16), "load 4.12  8 cpu");
        assert_eq!(join_fitting(&segments, 100), "load 4.12  8 cpu  temp 62C");
        // A field that does not fit ends the line; a shorter one behind it does
        // not jump the queue, because the order is the priority.
        let wide_then_narrow = vec!["a".repeat(40), "b".to_owned()];
        assert_eq!(join_fitting(&wide_then_narrow, 10), "");
    }

    #[test]
    fn the_scroll_offset_always_keeps_the_selection_visible() {
        for total in [0usize, 1, 5, 200] {
            for visible in [0usize, 1, 4, 30] {
                for selected in [0usize, 1, 3, 199] {
                    if selected >= total {
                        continue;
                    }
                    let offset = scroll_offset(Some(selected), visible, total);
                    if visible == 0 {
                        assert_eq!(offset, 0);
                        continue;
                    }
                    assert!(
                        offset <= selected,
                        "offset {offset} skipped past selection {selected}"
                    );
                    assert!(
                        selected < offset + visible,
                        "selection {selected} fell below the window at offset {offset}"
                    );
                }
            }
        }
        assert_eq!(scroll_offset(None, 10, 100), 0);
    }

    #[test]
    fn the_scroll_offset_does_not_move_within_the_first_window() {
        assert_eq!(scroll_offset(Some(0), 10, 100), 0);
        assert_eq!(scroll_offset(Some(9), 10, 100), 0);
        assert_eq!(scroll_offset(Some(10), 10, 100), 1);
        // It never scrolls past the end of the list.
        assert_eq!(scroll_offset(Some(99), 10, 100), 90);
    }

    #[test]
    fn splitting_rows_never_escapes_the_parent() {
        let area = Rect::new(3, 4, 20, 6);
        let rows = split_rows(area, &[2, 2, 2, 2]);
        assert_eq!(rows.len(), 4);
        for row in &rows {
            assert!(row.y >= area.y);
            assert!(row.y + row.height <= area.y + area.height);
        }
        assert!(rows[3].is_empty(), "the fourth row had no room left");
        // Zero area is legal and yields empty rectangles rather than panicking.
        for row in split_rows(Rect::new(0, 0, 0, 0), &[1, 1]) {
            assert!(row.is_empty());
        }
    }

    #[test]
    fn splitting_columns_never_escapes_the_parent() {
        let area = Rect::new(2, 2, 10, 3);
        let (left, right) = split_columns(area, 4);
        assert_eq!(left, Rect::new(2, 2, 4, 3));
        assert_eq!(right, Rect::new(6, 2, 6, 3));
        let (left, right) = split_columns(area, 99);
        assert_eq!(left, area);
        assert!(right.is_empty());
    }

    #[test]
    fn the_active_tab_is_bracketed_without_changing_the_strips_width() {
        let mut state = state_of(140, 38);
        let first = tab_segments(&state, 140);
        state = AppState::new(AppSettings {
            size: (140, 38),
            view: ViewId::Inspect,
            ..AppSettings::default()
        });
        let second = tab_segments(&state, 140);
        let width = |segments: &[(String, Token)]| -> usize {
            segments.iter().map(|(text, _)| display_width(text)).sum()
        };
        assert_eq!(
            width(&first),
            width(&second),
            "§5.4: switching view must not move the strip"
        );
        assert!(first.iter().any(|(text, _)| text == "[1 Overview]"));
        assert!(second.iter().any(|(text, _)| text == "[6 Inspect]"));
    }

    #[test]
    fn a_narrow_footer_condenses_the_tabs_to_digits() {
        let state = state_of(80, 24);
        let condensed = tab_segments(&state, 20);
        assert!(condensed.iter().any(|(text, _)| text == "[1]"));
        assert!(condensed.iter().all(|(text, _)| display_width(text) == 3));
    }

    #[test]
    fn the_hints_name_the_keys_the_active_keymap_binds() {
        let state = state_of(140, 38);
        let hints = hint_segments(&state);
        assert!(hints.iter().any(|hint| hint == "/ filter"));
        assert!(hints.iter().any(|hint| hint == "? help"));
        assert!(
            !hints.iter().any(|hint| hint.ends_with("live")),
            "a live view needs no return-to-live hint"
        );
    }

    #[test]
    fn a_frozen_timeline_gains_the_return_to_live_hint_first() {
        let mut state = state_of(140, 38);
        let _ = crate::app::reduce(&mut state, Action::TogglePause);
        assert!(state.timeline_status().is_frozen());
        let hints = hint_segments(&state);
        assert_eq!(
            hints.first().map(String::as_str),
            Some("L live"),
            "§2.1 makes returning to live one explicit action, so it leads"
        );
    }

    #[test]
    fn a_lagging_collector_is_named_in_the_header() {
        let mut state = state_of(140, 38);
        assert!(
            !header_segments(&state, None, 140)
                .iter()
                .any(|(text, _)| text.contains("lag"))
        );
        state.push_notice(crate::app::Notice::info(
            crate::app::NoticeKind::Collector,
            "test",
        ));
        // A health record the runtime would supply after a slow tick (§16.2).
        let health = CollectorHealth {
            lag: Duration::from_secs(4),
            ..CollectorHealth::default()
        };
        let _ = crate::app::apply(&mut state, crate::event::Event::<()>::health(health));
        assert!(
            header_segments(&state, None, 140)
                .iter()
                .any(|(text, _)| text.contains("lag")),
            "§16.2 requires collector lag to be displayed"
        );
    }

    #[test]
    fn a_sensor_reading_is_named_in_the_header_notes() {
        // §7.1's optional temperature and battery summary. The header's meter notes
        // are where it lives, because they are the one place present at every band
        // that has room for it at all.
        use monitrs_core::model::{BatterySnapshot, ChargeState, TemperatureReading};

        let mut snapshot =
            SystemSnapshot::warming_up(std::time::Instant::now(), SystemTime::UNIX_EPOCH, 8);
        assert!(
            !cpu_note_segments(&snapshot)
                .iter()
                .any(|segment| segment.contains("temp")),
            "a sensor that reported nothing must not occupy a field (§4)"
        );

        snapshot.sensors.temperatures = MetricState::Available(vec![TemperatureReading {
            label: "package".into(),
            celsius: 106.0,
            peak_celsius: Some(95.0),
            critical_celsius: Some(105.0),
        }]);
        snapshot.sensors.battery = MetricState::Available(BatterySnapshot {
            charge: Percent::new(82.0).expect("finite"),
            state: ChargeState::Discharging,
            time_remaining: MetricState::Unsupported,
            cycle_count: MetricState::Unsupported,
            capacity: MetricState::Unsupported,
            temperature_celsius: MetricState::Unsupported,
            power_watts: MetricState::Unsupported,
        });
        let cpu = cpu_note_segments(&snapshot);
        let temperature = cpu
            .iter()
            .find(|segment| segment.starts_with("temp"))
            .expect("the temperature field");
        assert!(
            temperature.contains('!'),
            "the sensor's own critical threshold must be flagged: {temperature}"
        );
        // The flag is the sensor's claim; §11.3 forbids concluding throttling.
        assert!(!temperature.contains("throttl"), "{temperature}");
        let memory = memory_note_segments(&snapshot, ByteUnits::Iec);
        assert!(
            memory.iter().any(|segment| segment.starts_with("bat 82%")),
            "{memory:?}"
        );
    }

    #[test]
    fn a_retained_temperature_is_shown_with_its_age_rather_than_disappearing() {
        // Sensors now have their own cadence, so a reading can legitimately be up
        // to 30 seconds old. Before this fix `hottest()` filtered to `fresh()` and
        // the header simply dropped the field for that window — worse than the
        // problem this release set out to fix.
        let mut snapshot =
            SystemSnapshot::warming_up(std::time::Instant::now(), SystemTime::UNIX_EPOCH, 8);
        snapshot.sensors.temperatures = MetricState::Stale {
            value: vec![TemperatureReading {
                label: "performance".into(),
                celsius: 62.0,
                peak_celsius: None,
                critical_celsius: Some(100.0),
            }],
            age: Duration::from_secs(28),
        };

        let segments = cpu_note_segments(&snapshot);
        let temperature = segments
            .iter()
            .find(|segment| segment.contains("62.0C"))
            .expect("the hottest reading must still be shown");
        // format_age renders 28 seconds as `00:28`, not `28s`; a test that only
        // looked for `62.0C` would pass even if the age were silently dropped.
        assert!(
            temperature.contains("~00:28"),
            "a retained reading must carry its age (§4), got {temperature}"
        );
    }

    #[test]
    fn a_measured_temperature_carries_no_age_marker() {
        let mut snapshot =
            SystemSnapshot::warming_up(std::time::Instant::now(), SystemTime::UNIX_EPOCH, 8);
        snapshot.sensors.temperatures = MetricState::Available(vec![TemperatureReading {
            label: "performance".into(),
            celsius: 62.0,
            peak_celsius: None,
            critical_celsius: Some(100.0),
        }]);

        let temperature = cpu_note_segments(&snapshot)
            .into_iter()
            .find(|segment| segment.contains("62.0C"))
            .expect("the hottest reading");
        assert!(!temperature.contains('~'), "got {temperature}");
    }

    #[test]
    fn the_header_truncates_the_host_name_rather_than_dropping_the_badge() {
        // §2.1 requires the header to display the timeline state, so the badge is
        // reserved and the host name — which is data — gives way.
        let state = state_of(80, 24);
        let mut snapshot =
            SystemSnapshot::warming_up(std::time::Instant::now(), SystemTime::UNIX_EPOCH, 8);
        snapshot.host.hostname = MetricState::Available(
            "an-extremely-long-build-agent-hostname.example.internal".into(),
        );
        // The narrowest header the §5.7 bands can produce is 60 cells wide minus the
        // reserved clock, so 30 is already below anything reachable.
        for budget in [30u16, 44, 60, 100, 200] {
            let segments = header_segments(&state, Some(&snapshot), budget);
            let mut row = RowBuilder::new(budget, crate::glyphs::GlyphSet::ascii());
            push_fitting(&mut row, Presentation::default(), &segments);
            let text = row.text();
            assert!(
                display_width(&text) <= usize::from(budget),
                "budget {budget} produced {} cells: {text:?}",
                display_width(&text)
            );
            assert!(
                text.contains("[>LIVE]"),
                "the timeline badge was dropped at budget {budget}: {text:?}"
            );
            // The host name is the field that gives way, and it gives way by
            // truncating rather than by disappearing while there is room for it.
            if (44..=60).contains(&budget) {
                assert!(text.contains("host:"), "budget {budget}: {text:?}");
                assert!(
                    !text.contains("example.internal"),
                    "the long name should be truncated at budget {budget}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn the_truncation_label_only_appears_when_rows_were_dropped() {
        assert_eq!(truncation_label(4, 4), None);
        assert_eq!(truncation_label(2, 9), Some("2 of 9".to_owned()));
    }

    #[test]
    fn the_history_span_label_reads_as_a_duration() {
        let state = state_of(140, 38);
        assert_eq!(history_span_label(state.history()), "5m");
    }

    #[test]
    fn the_caret_note_compares_the_selected_sample_with_its_baselines() {
        // A ring with a rising CPU curve, then a pause that pins the caret on the
        // newest sample. Sequence 0 is always `WarmingUp` regardless of pattern
        // (§8.2, §26), so a `Spike` lands the peak on the newest of three samples
        // and its `base` on the one before it — exactly the previous-sample
        // comparison this test is after.
        let scenario = monitrs_collectors::fake::Scenario {
            cpu: monitrs_collectors::fake::Pattern::Spike {
                base: 20.0,
                peak: 61.0,
                at: 2,
            },
            ..monitrs_collectors::fake::Scenario::default()
        };
        let mut state = fake_state(scenario, 3, (160, 48), ViewId::Overview);
        let _ = crate::app::reduce(&mut state, Action::TogglePause);

        let note = caret_note(&state, ByteUnits::Iec, 160);
        assert!(
            note.contains("cpu"),
            "the caret says what was selected; §2.5 asks it to say what changed: {note}"
        );
        assert!(
            note.contains("+41"),
            "the delta against the previous sample belongs in the note: {note}"
        );
    }

    #[test]
    fn a_baseline_history_cannot_reach_is_named_rather_than_shown_as_zero() {
        // Two samples, one second apart: nowhere near the 30-second look-back.
        let mut state = fake_state(
            monitrs_collectors::fake::Scenario::default(),
            2,
            (160, 48),
            ViewId::Overview,
        );
        let _ = crate::app::reduce(&mut state, Action::TogglePause);

        let note = caret_note(&state, ByteUnits::Iec, 160);
        assert!(
            note.contains("30s no baseline"),
            "two samples cannot reach thirty seconds back, and a missing baseline is \
             a word rather than a zero (§4, §26): {note}"
        );
    }

    // -----------------------------------------------------------------------
    // Containment and zero area (§5.7)
    // -----------------------------------------------------------------------

    /// A cell no screen produces, so a write that escapes its rectangle is caught.
    fn sentinel_cell() -> ratatui::buffer::Cell {
        let mut cell = ratatui::buffer::Cell::EMPTY;
        cell.set_symbol("\u{2603}");
        cell.set_style(
            ratatui::style::Style::new()
                .fg(ratatui::style::Color::Rgb(1, 2, 3))
                .bg(ratatui::style::Color::Rgb(4, 5, 6))
                .add_modifier(ratatui::style::Modifier::SLOW_BLINK),
        );
        cell
    }

    /// A state fed `samples` deterministic snapshots from the fake collector.
    ///
    /// The fake is a dev-dependency for exactly this: §17.3 wants frames of states
    /// a real machine cannot be put into on demand, and hand-built snapshots would
    /// let a test drift away from what a collector can actually produce.
    fn fake_state(
        scenario: monitrs_collectors::fake::Scenario,
        samples: u64,
        size: (u16, u16),
        view: ViewId,
    ) -> AppState {
        use monitrs_collectors::fake::FakeCollector;
        use monitrs_collectors::source::{SampleTick, SnapshotSource};
        use monitrs_collectors::tier::DueTiers;

        let mut state = AppState::new(AppSettings {
            size,
            view,
            ..AppSettings::default()
        });
        let mut collector = FakeCollector::new(scenario);
        let start = std::time::Instant::now();
        let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        for index in 0..samples {
            if index > 0 {
                tick = tick.advance(
                    start + Duration::from_secs(index),
                    SystemTime::UNIX_EPOCH + Duration::from_secs(index),
                    DueTiers::ALL,
                );
            }
            if let Ok(snapshot) = collector.sample(&tick) {
                let _ = crate::app::apply(
                    &mut state,
                    crate::event::Event::<()>::Snapshot(std::sync::Arc::new(snapshot)),
                );
            }
        }
        state
    }

    /// Renders `state`'s view into `area` inside a larger sentinel-filled buffer.
    fn render_into(
        state: &AppState,
        presentation: Presentation<'_>,
        buffer_size: (u16, u16),
        area: Rect,
    ) -> Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(buffer_size.0, buffer_size.1))
            .expect("a test backend never fails to initialise");
        let _ = terminal.draw(|frame| {
            let full = frame.area();
            let sentinel = sentinel_cell();
            let buffer = frame.buffer_mut();
            for y in full.top()..full.bottom() {
                for x in full.left()..full.right() {
                    if let Some(cell) = buffer.cell_mut((x, y)) {
                        *cell = sentinel.clone();
                    }
                }
            }
            render(frame, area, state, presentation);
        });
        terminal.backend().buffer().clone()
    }

    fn assert_margin_untouched(buffer: &Buffer, area: Rect) {
        let sentinel = sentinel_cell();
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..buffer.area.right() {
                if area.contains((x, y).into()) {
                    continue;
                }
                let cell = buffer.cell((x, y)).expect("inside the buffer");
                assert_eq!(
                    cell, &sentinel,
                    "a screen wrote to ({x}, {y}), outside {area:?}"
                );
            }
        }
    }

    fn presentations() -> [Presentation<'static>; 3] {
        use crate::glyphs::GlyphSet;
        use crate::theme::{ColorDepth, ThemeId};
        [
            Presentation::new(
                GlyphSet::ascii(),
                ThemeId::DefaultDark.theme(),
                ColorDepth::TrueColor,
            ),
            Presentation::new(
                GlyphSet::unicode(),
                ThemeId::DefaultDark.theme(),
                ColorDepth::TrueColor,
            ),
            Presentation::new(
                GlyphSet::ascii(),
                ThemeId::HighContrast.theme(),
                ColorDepth::Off,
            ),
        ]
    }

    #[test]
    fn every_screen_stays_inside_its_rectangle_at_every_breakpoint() {
        // §5.7's four bands plus the two boundaries that decide them.
        const MARGIN: u16 = 2;
        let sizes = [
            (140u16, 38u16),
            (100, 28),
            (80, 24),
            (60, 16),
            (52, 12),
            (200, 60),
        ];
        for (width, height) in sizes {
            for view in ViewId::ALL {
                let state = fake_state(
                    monitrs_collectors::fake::Scenario::default(),
                    3,
                    (width, height),
                    view,
                );
                for presentation in presentations() {
                    let area = Rect::new(MARGIN, MARGIN, width, height);
                    let buffer = render_into(
                        &state,
                        presentation,
                        (width + MARGIN * 2, height + MARGIN * 2),
                        area,
                    );
                    assert_margin_untouched(&buffer, area);
                }
            }
        }
    }

    #[test]
    fn a_zero_area_frame_draws_nothing_and_never_panics() {
        // §5.7 makes this a hard requirement, and a resize storm really produces it.
        for (width, height) in [(0u16, 0u16), (0, 24), (200, 0), (1, 1), (3, 2)] {
            for view in ViewId::ALL {
                let state = fake_state(
                    monitrs_collectors::fake::Scenario::default(),
                    2,
                    (width, height),
                    view,
                );
                for presentation in presentations() {
                    let area = Rect::new(2, 2, width, height);
                    let buffer = render_into(&state, presentation, (width + 6, height + 6), area);
                    assert_margin_untouched(&buffer, area);
                }
            }
        }
    }

    #[test]
    fn a_screen_with_no_snapshot_at_all_still_renders() {
        // The first frame: the runtime draws before the collector has answered, and
        // §26 says the first sample is warming up rather than zero.
        for view in ViewId::ALL {
            let state = AppState::new(AppSettings {
                size: (140, 38),
                view,
                ..AppSettings::default()
            });
            let area = Rect::new(1, 1, 140, 38);
            let buffer = render_into(&state, Presentation::default(), (142, 40), area);
            assert_margin_untouched(&buffer, area);
        }
    }

    #[test]
    fn the_header_describes_the_selected_sample_rather_than_the_frozen_one() {
        // §5.6: while a specific earlier sample is selected the header meters are
        // *that* sample's, and the notes say so. A row that mixed the selected
        // offset with a live value would be the confusion §26 forbids.
        let mut state = fake_state(
            monitrs_collectors::fake::Scenario::default(),
            30,
            (140, 38),
            ViewId::Overview,
        );
        let live = render_into(&state, Presentation::default(), (140, 38), state.area());
        for _ in 0..8 {
            let _ = crate::app::reduce(
                &mut state,
                Action::SeekHistory(crate::action::Seek::Backward(1)),
            );
        }
        let scrubbed = render_into(&state, Presentation::default(), (140, 38), state.area());

        let header_of = |buffer: &Buffer| -> String {
            (0..140)
                .filter_map(|x| buffer.cell((x, 1)).map(|cell| cell.symbol().to_owned()))
                .collect()
        };
        assert_ne!(
            header_of(&live),
            header_of(&scrubbed),
            "the header must change when a sample is selected"
        );
        assert!(
            header_of(&scrubbed).contains("behind live")
                || header_of(&scrubbed).contains("sample ")
        );
        // The live note fields are gone: nothing on the row is a current reading.
        assert!(!header_of(&scrubbed).contains("8 cpu"));
    }

    #[test]
    fn a_frozen_frame_is_distinguishable_from_a_live_one_without_colour() {
        // §26: historical state and live state must be visually unmistakable, and
        // §5.2 forbids colour from carrying that on its own.
        let mut state = fake_state(
            monitrs_collectors::fake::Scenario::default(),
            12,
            (140, 38),
            ViewId::Overview,
        );
        let area = Rect::new(0, 0, 140, 38);
        let plain = Presentation::default();
        let live = render_into(&state, plain, (140, 38), area);
        let _ = crate::app::reduce(&mut state, Action::TogglePause);
        let frozen = render_into(&state, plain, (140, 38), area);

        let text = |buffer: &Buffer| -> String {
            (buffer.area.top()..buffer.area.bottom())
                .flat_map(|y| {
                    (buffer.area.left()..buffer.area.right()).filter_map(move |x| {
                        buffer.cell((x, y)).map(|cell| cell.symbol().to_owned())
                    })
                })
                .collect()
        };
        let live_text = text(&live);
        let frozen_text = text(&frozen);
        assert_ne!(live_text, frozen_text);
        assert!(live_text.contains("LIVE"), "{live_text}");
        assert!(frozen_text.contains("PAUSED"), "{frozen_text}");
        assert!(
            !frozen_text.contains("LIVE") || frozen_text.contains("L live"),
            "the only `live` left is the return-to-live hint"
        );
    }
}
