//! The Storage screen (§7.3): filesystem capacity, device throughput, the processes
//! responsible for the traffic, and the throughput history.
//!
//! ```text
//! + FILESYSTEM CAPACITY ---- 2 mounted, = shares a device, SIZE is not additive -+
//! | MOUNT              DEVICE   TYPE     SIZE  USED AVAIL  USE% INODE%  IFREE     |
//! |=/                  disk0s1  apfs ro  460G  424G   37G   92%   0.3%   144M ... |
//! |=/System/...es/Data disk0s1  apfs     460G  424G   37G   92%   2.4%   144M ... |
//! + DEVICE THROUGHPUT ------------------------------- busy - (unsupported) ------+
//! |DEVICE      READ/s WRITE/s   R-IOPS   W-IOPS MODEL       MOUNTS               |
//! |disk0       3.2M/s   38M/s      n/a      n/a -           / /System/Volumes/... |
//! + TOP DISK I/O ---------------------------------------------- 31 of 982 -------+
//! | READ/s WRITE/s TOTAL R TOTAL W      PID NAME                                 |
//! | 5.3M/s  2.8M/s    3.4M    1.5M    14700 rustc                                |
//! |   0B/s    0B/s     18G     30G    84322 Google Chrome                        |
//! + THROUGHPUT HISTORY 5m ------------ all devices, write peak 38M/s ------------+
//! | READ                                              .....::-=+*##@%#*+=--:...  |
//! | WRITE                                             ====+++++*********######## |
//! ```
//!
//! # Why four sections and never one number
//!
//! §7.3 and §26 both say it outright: filesystem capacity and device utilization
//! are different metrics. A filesystem that is 95% full is not busy, and a device
//! saturated at 100% utilization may sit on a nearly empty one. `monitrs-core`
//! keeps them in two types with no overlapping fields, and this screen keeps them
//! in two panels with two headings — so there is no code path that could render
//! "76%" without saying 76% *of what*.
//!
//! The two lower panels answer the question the upper two raise. `TOP DISK I/O` is
//! the whole reason a storage screen exists — *what is writing to my disk right
//! now* — and no other screen ranks [`ProcessSnapshot::io`], which is collected on
//! every fast tick and was previously visible only in the Inspect screen's detail of
//! one selected process. The history panel is the same aggregate the Overview plots,
//! at the width this screen can give it.
//!
//! # Device busy is shown only where it is real
//!
//! §7.3 restricts busy/utilization to platforms "where it is semantically
//! correct". The gate is [`CapabilitySnapshot::disk_busy`]: when the platform
//! cannot produce it, the `BUSY` and `QUEUE` columns are not drawn at all and the
//! panel's trailing label says so. Reserving two columns to print `n/a` on every
//! row would spend the width that the throughput figures need, and §4 explicitly
//! allows an `Unsupported` optional field to be hidden when space is scarce.
//!
//! # Inodes, and why they are not optional information
//!
//! A filesystem can be out of inodes with hundreds of gigabytes free, and then every
//! `create` fails with `ENOSPC` while `USE%` reads a comfortable 40%. That is the
//! classic operational surprise no other panel here would catch, so `INODE%` sits
//! beside `USE%` — two percentages of two different things, each under its own
//! heading, which is the same discipline the capacity/throughput split follows. Many
//! filesystems have no inode table at all; those read `n/a` and never `0%`, because
//! [`InodeUsage::from_counts`] refuses to turn a zero table size into a number (§4).
//!
//! # Shared capacity is a presentation problem, so it is solved here
//!
//! Two mounts on an APFS Mac — `/` and `/System/Volumes/Data` — report the same 494G
//! because they share one container, and a reader who adds the `SIZE` column gets
//! 988G of disk that does not exist. Nothing in the *data* is wrong, so nothing in
//! the model changes: the rows whose device is shared are marked with a `=`, and the
//! trailing label says what the mark means (§10.1 keeps this kind of decision in
//! `monitrs-tui`, §5.2 gives it a character rather than a colour).
//!
//! # Why the history is not per device
//!
//! §8.5's ring keeps one aggregate read series and one aggregate write series, not
//! one pair per device. Plotting a per-device line would mean either five more rings
//! — which §16.1's memory budget does not invite without a measurement first — or
//! fabricating a series from the single sample this frame holds, which §4 forbids
//! outright. So the plot is labelled as the machine total, and the per-device figures
//! stay in `DEVICE THROUGHPUT` where they are measured.
//!
//! # Removable and virtual filtering
//!
//! §7.3 asks for removable/virtual filtering. Kernel pseudo-filesystems —
//! everything [`FilesystemKind::hidden_by_default`] covers — are hidden, and the
//! count of hidden mounts goes in the trailing label, because a filter the reader
//! cannot see is indistinguishable from a collector that missed them. Removable
//! mounts are shown and labelled: they are real storage, and which of them is
//! nearly full is exactly the question this screen answers.
//!
//! [`CapabilitySnapshot::disk_busy`]: monitrs_core::model::CapabilitySnapshot::disk_busy
//! [`FilesystemKind::hidden_by_default`]: monitrs_core::model::FilesystemKind::hidden_by_default
//! [`InodeUsage::from_counts`]: monitrs_core::model::InodeUsage::from_counts
//! [`ProcessSnapshot::io`]: monitrs_core::model::ProcessSnapshot::io

use monitrs_core::history::HistoryMetric;
use monitrs_core::model::{
    CapabilityState, DiskSnapshot, FilesystemKind, FilesystemSnapshot, ProcessSnapshot,
    SystemSnapshot,
};
use monitrs_core::units::{
    MAX_BYTE_RATE_WIDTH, MAX_COMPACT_BYTES_WIDTH, display_width, format_bytes_compact,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Borders, Widget};

use crate::app::AppState;
use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::Presentation;
use crate::widgets::states;
use crate::widgets::{Sparkline, SparklineCaret};

use super::{
    Chrome, SHARED_BOTTOM, caret_note, draw_bordered_panel, history_span_label, inner_of, inset,
    muted_line, plot_peak, plot_series, row_builder, selected_sample_offset, split_rows,
    truncation_label, write_lines,
};

/// Cells reserved for a mount point before it is middle-truncated.
///
/// §5.4 truncates paths from the middle, because a mount's leading directory and
/// its leaf both identify it.
const MOUNT_WIDTH: u16 = 18;

/// Cells reserved for a device name.
const DEVICE_WIDTH: u16 = 10;

/// Cells reserved for a filesystem type.
const FS_TYPE_WIDTH: u16 = 8;

/// Cells reserved for a byte figure, from the formatter's own bound (§5.4).
const BYTES_WIDTH: u16 = MAX_COMPACT_BYTES_WIDTH;

/// Cells reserved for a throughput figure, from the formatter's own bound.
const RATE_WIDTH: u16 = MAX_BYTE_RATE_WIDTH;

/// Cells reserved for a percentage.
const PERCENT_WIDTH: u16 = 5;

/// Cells reserved for an IOPS figure.
const IOPS_WIDTH: u16 = 8;

/// Cells reserved for a device model string.
const MODEL_WIDTH: u16 = 16;

/// Cells reserved for the inode percentage.
///
/// One wider than [`PERCENT_WIDTH`], which buys two things: the `INODE%` heading
/// fits without abbreviation, and a refused inode read renders as `denied` rather
/// than degrading to the `n/a` that also means "this filesystem has no inode
/// table" (§4's abbreviation rung exists for exactly this distinction).
const INODE_PERCENT_WIDTH: u16 = 6;

/// Cells reserved for the free-inode count.
///
/// Six, for the same `denied` reason: [`format_count_compact`] never needs more
/// than five.
const INODE_FREE_WIDTH: u16 = 6;

/// Cells reserved for a PID, which fits every `pid_max` in use.
const PID_WIDTH: u16 = 8;

/// Cells reserved for the labels down the left of the history panel.
const HISTORY_LABEL_WIDTH: u16 = 7;

/// The narrowest capacity meter worth drawing beside a filesystem row.
const MIN_CAPACITY_METER: u16 = 8;

/// The narrowest capacity row that keeps the inode columns.
///
/// Below this the byte figures win the cells. They are what a reader looks for
/// first, and §4 permits hiding an optional field where space is scarce. The panel
/// says so in its trailing label while there is room for the sentence — see
/// [`fit_label`], which spends the last cells on the higher-priority disclosures.
const MIN_INODE_COLUMNS: u16 = 96;

/// The character that marks a filesystem sharing its device with another.
///
/// §5.2: the mark is a character first. It is `=` because the rows it joins are
/// *equal* by construction — they report the same total, because it is the same
/// total.
const SHARED_MARKER: char = '=';

/// Rows the history panel asks for: a border, two plots, and a border.
const HISTORY_HEIGHT: u16 = 4;

/// The fewest rows worth giving the top-I/O panel before the history is dropped.
///
/// A border, a header, and two processes. Below that the panel would name one
/// process and imply it is the only one, which is worse than not drawing the
/// history — the history is also on the Overview, and this ranking is nowhere else.
const MIN_TOP_IO_ROWS: u16 = 4;

/// Draws the Storage screen (§7.3).
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

    // Each of the two upper panels takes exactly the rows its own contents need, and
    // the process ranking takes what is left — the same order of priority the CPU
    // screen uses, and for the same reason: sizing these by share of the screen left
    // thirty blank rows under a two-mount table, which is what made this screen worth
    // complaining about. The caps keep a container host with fifty mounts from
    // squeezing the ranking out; `truncation_label` then says how many rows it lost.
    let (shown, _) = partition_filesystems(snapshot);
    let capacity_want = panel_height(shown.len());
    let device_want = panel_height(snapshot.disks.len());
    let capacity_rows = capacity_want.min(half_of(body.height)).min(body.height);
    let remaining = body.height.saturating_sub(capacity_rows);
    let device_rows = device_want.min(third_of(body.height)).min(remaining);
    let remaining = remaining.saturating_sub(device_rows);
    let history_rows = if remaining >= HISTORY_HEIGHT.saturating_add(MIN_TOP_IO_ROWS) {
        HISTORY_HEIGHT
    } else {
        0
    };
    let top_io_rows = remaining.saturating_sub(history_rows);
    let rows = split_rows(
        body,
        &[capacity_rows, device_rows, top_io_rows, history_rows],
    );

    // §5.5 shares the row between vertically adjacent panels: a panel's bottom edge
    // is the next panel's top rule, so the duplicate is omitted rather than spending
    // a row of content on it. Whichever panel is last keeps its own bottom, because
    // the status footer is not a panel.
    let last = rows
        .iter()
        .rposition(|area| area.height >= 3)
        .unwrap_or_default();
    let borders_for = |index: usize| {
        if index == last {
            Borders::ALL
        } else {
            SHARED_BOTTOM
        }
    };

    if let Some(area) = rows.first() {
        draw_capacity(buffer, *area, snapshot, presentation, borders_for(0));
    }
    if let Some(area) = rows.get(1) {
        draw_throughput(buffer, *area, snapshot, presentation, borders_for(1));
    }
    if let Some(area) = rows.get(2).filter(|area| area.height >= 2) {
        draw_top_io(buffer, *area, state, snapshot, presentation, borders_for(2));
    }
    if let Some(area) = rows.get(3).filter(|area| area.height >= 3) {
        draw_history(buffer, *area, state, presentation, borders_for(3));
    }
}

/// The rows a table panel wants: a top rule, a column header, and one row each.
fn panel_height(rows: usize) -> u16 {
    let rows = u16::try_from(rows).unwrap_or(u16::MAX);
    // At least one body row even with nothing to show, so the "nothing here, and
    // here is why" line has somewhere to go.
    rows.max(1).saturating_add(2)
}

/// Half the body, but never less than a panel's own frame needs.
fn half_of(height: u16) -> u16 {
    (height / 2).max(3)
}

/// A third of the body, but never less than a panel's own frame needs.
fn third_of(height: u16) -> u16 {
    (height / 3).max(3)
}

/// The capacity panel's heading, named once because [`fit_label`] has to reserve its
/// width from the panel's own geometry rather than from the label's length (§5.4).
const CAPACITY_TITLE: &str = "FILESYSTEM CAPACITY";

/// Draws the filesystem-capacity section (§7.3).
fn draw_capacity(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let (shown, hidden) = partition_filesystems(snapshot);
    let probe = inner_of(presentation, area, borders);
    let body_rows = usize::from(probe.height.saturating_sub(1));
    let show_inodes = probe.width >= MIN_INODE_COLUMNS;
    let shared = shared_devices(&shown);

    let mut parts = vec![format!("{} mounted", shown.len())];
    if let Some(cut) = truncation_label(body_rows.min(shown.len()), shown.len()) {
        parts = vec![format!("{cut} mounted")];
    }
    if hidden > 0 {
        // A filter the reader cannot see is indistinguishable from a collector that
        // missed the mounts (§7.3's filtering must be disclosed).
        parts.push(format!("{hidden} virtual hidden"));
    }
    if !shared.is_empty() {
        // The mark first, because a mark nobody can decode is noise; the consequence
        // second, because it is the sentence that stops a reader adding 494G to 494G.
        parts.push(format!("{SHARED_MARKER} shares a device"));
        parts.push("SIZE is not additive".to_owned());
    }
    if !show_inodes {
        parts.push("inodes need a wider terminal".to_owned());
    }
    let trailing = fit_label(&parts, CAPACITY_TITLE, area.width);

    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        CAPACITY_TITLE,
        Some(trailing.as_str()),
        false,
        borders,
    );
    if inner.is_empty() {
        return;
    }
    let meter_width = capacity_meter_width(inner.width, show_inodes);
    let mut lines = vec![capacity_header(
        presentation,
        inner.width,
        meter_width,
        show_inodes,
    )];
    for filesystem in shown.iter().take(body_rows) {
        lines.push(capacity_row(
            presentation,
            inner.width,
            filesystem,
            meter_width,
            show_inodes,
            is_shared(filesystem, &shared),
        ));
    }
    if shown.is_empty() {
        lines.push(muted_line(
            presentation,
            inner.width,
            "no filesystem capacity reported",
        ));
    }
    write_lines(buffer, inner, &lines);
}

/// Joins the capacity panel's disclosures, dropping the ones that do not fit.
///
/// [`crate::widgets::Panel`] drops its trailing label *whole* rather than truncating
/// it, which is right for a count and wrong for a sentence: at 80 columns the full
/// text does not fit, and without this the mount count and the hidden-mount
/// disclosure would vanish along with the part that overflowed. So the parts are
/// fitted here in priority order — what is on screen, what the filter removed, what
/// the marks mean, then why a column is missing — and a part that does not fit ends
/// the label rather than letting a shorter one behind it jump the queue (§5.4).
fn fit_label(parts: &[String], title: &str, panel_width: u16) -> String {
    // The corners, the rule either side of the title, and the three cells the panel
    // keeps before its right corner. Deliberately pessimistic: a label dropped one
    // cell early is a shorter sentence, while one kept a cell too late is dropped
    // whole by the panel, which is the outcome this function exists to avoid.
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

/// The mounts to show, and how many were hidden by the §7.3 filter.
fn partition_filesystems(snapshot: &SystemSnapshot) -> (Vec<&FilesystemSnapshot>, usize) {
    let (hidden, shown): (Vec<_>, Vec<_>) = snapshot
        .filesystems
        .iter()
        .partition(|filesystem| filesystem.kind.hidden_by_default());
    (shown, hidden.len())
}

/// The devices that back more than one of the *shown* mounts.
///
/// Only the shown ones, because the mark answers a question about this table: two
/// rows on screen claiming the same capacity. A device shared with a mount the §7.3
/// filter hid is not something the reader can double-count.
///
/// Sorted rather than compared pairwise: a container host can mount hundreds of
/// filesystems, and this runs once per frame — an `n²` scan over 500 mounts is real
/// milliseconds inside §16.1's frame budget, for an answer `n log n` gives.
fn shared_devices(shown: &[&FilesystemSnapshot]) -> Vec<Box<str>> {
    // A mount whose device the platform did not name is skipped entirely: two
    // unnamed devices are not evidence of one device, and marking them would invent
    // the very claim the mark makes.
    let mut devices: Vec<&str> = shown
        .iter()
        .filter_map(|filesystem| filesystem.device.as_deref())
        .collect();
    devices.sort_unstable();
    let mut shared: Vec<Box<str>> = Vec::new();
    for pair in devices.windows(2) {
        let [first, second] = pair else { continue };
        if first == second && shared.last().is_none_or(|last| &**last != *first) {
            shared.push(Box::from(*first));
        }
    }
    shared
}

/// Whether this mount's device backs another shown mount as well.
fn is_shared(filesystem: &FilesystemSnapshot, shared: &[Box<str>]) -> bool {
    filesystem
        .device
        .as_deref()
        .is_some_and(|device| shared.iter().any(|known| &**known == device))
}

/// Cells the capacity meter gets, or zero when the row is too narrow for one.
fn capacity_meter_width(width: u16, show_inodes: bool) -> u16 {
    let inodes = if show_inodes {
        INODE_PERCENT_WIDTH + INODE_FREE_WIDTH + 2
    } else {
        0
    };
    let fixed = 1
        + MOUNT_WIDTH
        + DEVICE_WIDTH
        + FS_TYPE_WIDTH
        + BYTES_WIDTH * 3
        + PERCENT_WIDTH
        + inodes
        + 8;
    let spare = width.saturating_sub(fixed);
    if spare >= MIN_CAPACITY_METER {
        spare.min(24)
    } else {
        0
    }
}

/// The capacity section's column header.
fn capacity_header(
    presentation: Presentation<'_>,
    width: u16,
    meter: u16,
    show_inodes: bool,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    let muted = presentation.style(Token::Muted);
    // One cell for the shared-device mark, so every row below lines up with the
    // heading whether it carries a mark or not (§5.4: a state must not shift a row).
    row.pad(1);
    for (text, cells, align) in [
        ("MOUNT", MOUNT_WIDTH, Align::Left),
        ("DEVICE", DEVICE_WIDTH, Align::Left),
        ("TYPE", FS_TYPE_WIDTH, Align::Left),
        ("SIZE", BYTES_WIDTH, Align::Right),
        ("USED", BYTES_WIDTH, Align::Right),
        ("AVAIL", BYTES_WIDTH, Align::Right),
        ("USE%", PERCENT_WIDTH, Align::Right),
    ] {
        row.push_field(text, cells, align, muted);
        row.pad(1);
    }
    if show_inodes {
        row.push_field("INODE%", INODE_PERCENT_WIDTH, Align::Right, muted);
        row.pad(1);
        row.push_field("IFREE", INODE_FREE_WIDTH, Align::Right, muted);
        row.pad(1);
    }
    if meter > 0 {
        row.push_field("CAPACITY", meter, Align::Left, muted);
    }
    row.finish()
}

/// One filesystem's capacity row.
fn capacity_row(
    presentation: Presentation<'_>,
    width: u16,
    filesystem: &FilesystemSnapshot,
    meter: u16,
    show_inodes: bool,
    shared: bool,
) -> Line<'static> {
    let units = presentation.units();
    let glyphs = presentation.glyphs();
    let mut row = row_builder(presentation, width);
    let text = presentation.style(Token::Text);
    let muted = presentation.style(Token::Muted);

    row.push_field(
        &if shared {
            SHARED_MARKER.to_string()
        } else {
            String::new()
        },
        1,
        Align::Left,
        muted,
    );
    // §5.4: a path is truncated from the middle, because both ends identify it.
    row.push_field(
        &states::fit_middle_within(&filesystem.mount_point, usize::from(MOUNT_WIDTH), glyphs),
        MOUNT_WIDTH,
        Align::Left,
        text,
    );
    row.pad(1);
    row.push_field(
        filesystem.device.as_deref().unwrap_or("-"),
        DEVICE_WIDTH,
        Align::Left,
        muted,
    );
    row.pad(1);
    row.push_field(
        &filesystem_kind_text(filesystem),
        FS_TYPE_WIDTH,
        Align::Left,
        muted,
    );
    row.pad(1);
    row.push_field(
        &format_bytes_compact(filesystem.total_bytes, units),
        BYTES_WIDTH,
        Align::Right,
        text,
    );
    row.pad(1);
    for state in [&filesystem.used_bytes, &filesystem.available_bytes] {
        let display = states::describe_bytes(state, units);
        row.push_field(
            &display.fitted(usize::from(BYTES_WIDTH), glyphs),
            BYTES_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    let usage = states::describe_percent(&filesystem.usage);
    row.push_field(
        &usage.fitted(usize::from(PERCENT_WIDTH), glyphs),
        PERCENT_WIDTH,
        Align::Right,
        presentation.metric_style(&usage),
    );
    row.pad(1);
    if show_inodes {
        // Two percentages of two different things, and the heading above each says
        // which: §7.3's rule about never mixing metrics applies inside a panel too.
        let inodes = states::describe_percent(&filesystem.inode_usage());
        row.push_field(
            &inodes.fitted(usize::from(INODE_PERCENT_WIDTH), glyphs),
            INODE_PERCENT_WIDTH,
            Align::Right,
            presentation.metric_style(&inodes),
        );
        row.pad(1);
        let free = states::describe(&filesystem.inodes, |inodes| {
            format_count_compact(inodes.free())
        });
        row.push_field(
            &free.fitted(usize::from(INODE_FREE_WIDTH), glyphs),
            INODE_FREE_WIDTH,
            Align::Right,
            presentation.metric_style(&free),
        );
        row.pad(1);
    }
    if meter > 0 {
        // §4: an unmeasured capacity draws the track glyph, never an empty bar,
        // because an empty bar means "measured, and it is empty".
        let bar = match filesystem.usage.displayable() {
            Some((percent, _)) => glyphs.meter(percent.fraction(), usize::from(meter)),
            None => glyphs.unknown_meter(usize::from(meter)),
        };
        row.push_field(&bar, meter, Align::Left, presentation.metric_style(&usage));
    }
    row.finish()
}

/// A count in the compact form a six-cell column has room for, such as `4.4M`.
///
/// Presentation, so it lives here rather than in `monitrs-core` (§10.1): an inode
/// count is a plain number, and the decision to abbreviate it — in powers of ten,
/// because an inode table is sized and reported in decimal, unlike the byte columns
/// beside it — belongs to the screen that has six cells to say it in. Counts below
/// ten thousand are exact, because there the digits are the information.
///
/// Bounded at `T`: a count of 10^15 or more renders wider than the column and is
/// then tail-truncated by the row builder. No inode table is within a thousandfold of
/// that, and inventing another suffix for a number no filesystem reports would be
/// vocabulary nobody could read.
fn format_count_compact(count: u64) -> String {
    const SCALES: [(u64, char); 4] = [
        (1_000_000_000_000, 'T'),
        (1_000_000_000, 'G'),
        (1_000_000, 'M'),
        (1_000, 'K'),
    ];
    if count < 10_000 {
        return count.to_string();
    }
    for (scale, suffix) in SCALES {
        if count >= scale {
            // Narrowing to f64 is exact for every count below 2^53, which is every
            // inode table a filesystem can have.
            #[allow(clippy::cast_precision_loss)]
            let value = count as f64 / scale as f64;
            return if value < 10.0 {
                format!("{value:.1}{suffix}")
            } else {
                format!("{value:.0}{suffix}")
            };
        }
    }
    count.to_string()
}

/// The filesystem type, with the read-only and removable facts folded in.
fn filesystem_kind_text(filesystem: &FilesystemSnapshot) -> String {
    let base = filesystem.fs_type.as_deref().unwrap_or("-");
    let mut text = base.to_owned();
    if filesystem.kind == FilesystemKind::Removable {
        text.push('*');
    }
    if filesystem.kind == FilesystemKind::Network {
        text.push('@');
    }
    if filesystem.read_only {
        text.push_str(" ro");
    }
    text
}

/// Draws the device-throughput section (§7.3).
fn draw_throughput(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let busy = snapshot.capabilities.disk_busy;
    let show_busy = busy == CapabilityState::Available;
    let probe = inner_of(presentation, area, borders);
    let body_rows = usize::from(probe.height.saturating_sub(1));
    let trailing = if show_busy {
        truncation_label(body_rows.min(snapshot.disks.len()), snapshot.disks.len())
    } else {
        // §7.3: busy is shown only where it is semantically correct, and §4 wants
        // the reason rather than a blank column.
        Some(format!("busy {} ({})", busy.symbol(), busy.label()))
    };

    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        "DEVICE THROUGHPUT",
        trailing.as_deref(),
        false,
        borders,
    );
    if inner.is_empty() {
        return;
    }
    let mut lines = vec![throughput_header(presentation, inner.width, show_busy)];
    for device in snapshot.disks.iter().take(body_rows) {
        lines.push(throughput_row(presentation, inner.width, device, show_busy));
    }
    if snapshot.disks.is_empty() {
        lines.push(muted_line(
            presentation,
            inner.width,
            "no block device counters reported",
        ));
    }
    write_lines(buffer, inner, &lines);
}

/// The throughput section's column header.
fn throughput_header(presentation: Presentation<'_>, width: u16, show_busy: bool) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    let muted = presentation.style(Token::Muted);
    for (text, cells, align) in [
        ("DEVICE", DEVICE_WIDTH, Align::Left),
        ("READ/s", RATE_WIDTH, Align::Right),
        ("WRITE/s", RATE_WIDTH, Align::Right),
        ("R-IOPS", IOPS_WIDTH, Align::Right),
        ("W-IOPS", IOPS_WIDTH, Align::Right),
    ] {
        row.push_field(text, cells, align, muted);
        row.pad(1);
    }
    if show_busy {
        row.push_field("BUSY", PERCENT_WIDTH, Align::Right, muted);
        row.pad(1);
        row.push_field("QUEUE", PERCENT_WIDTH, Align::Right, muted);
        row.pad(1);
    }
    row.push_field("MODEL", MODEL_WIDTH, Align::Left, muted);
    row.pad(1);
    let remaining = row.remaining();
    row.push_field("MOUNTS", remaining, Align::Left, muted);
    row.finish()
}

/// One device's throughput row.
fn throughput_row(
    presentation: Presentation<'_>,
    width: u16,
    device: &DiskSnapshot,
    show_busy: bool,
) -> Line<'static> {
    let units = presentation.units();
    let glyphs = presentation.glyphs();
    let mut row = row_builder(presentation, width);
    row.push_field(
        &device.device,
        DEVICE_WIDTH,
        Align::Left,
        presentation.style(Token::Text),
    );
    row.pad(1);
    for state in [&device.read, &device.write] {
        let display = states::describe_byte_rate(state, units);
        row.push_field(
            &display.fitted(usize::from(RATE_WIDTH), glyphs),
            RATE_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    for state in [&device.read_ops, &device.write_ops] {
        let display = states::describe(state, |rate| format!("{:.0}", rate.per_second()));
        row.push_field(
            &display.fitted(usize::from(IOPS_WIDTH), glyphs),
            IOPS_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    if show_busy {
        let busy = states::describe_percent(&device.busy);
        row.push_field(
            &busy.fitted(usize::from(PERCENT_WIDTH), glyphs),
            PERCENT_WIDTH,
            Align::Right,
            presentation.metric_style(&busy),
        );
        row.pad(1);
        let queue = states::describe(&device.queue_length, |value| format!("{value:.1}"));
        row.push_field(
            &queue.fitted(usize::from(PERCENT_WIDTH), glyphs),
            PERCENT_WIDTH,
            Align::Right,
            presentation.metric_style(&queue),
        );
        row.pad(1);
    }
    row.push_field(
        device.model.as_deref().unwrap_or("-"),
        MODEL_WIDTH,
        Align::Left,
        presentation.style(Token::Muted),
    );
    row.pad(1);
    let mounts = if device.mount_points.is_empty() {
        // §8.6 puts the device-to-mount mapping in the on-demand tier, so an empty
        // list means "not mapped yet", not "mounted nowhere".
        "not mapped".to_owned()
    } else {
        device.mount_points.join(" ")
    };
    let remaining = row.remaining();
    row.push_field(
        &states::fit_middle_within(&mounts, usize::from(remaining), glyphs),
        remaining,
        Align::Left,
        presentation.style(Token::Muted),
    );
    row.finish()
}

/// Draws the per-process I/O ranking: what is actually using the disk.
fn draw_top_io(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let capability = snapshot.capabilities.per_process_io;
    let ranked = ranked_by_io(snapshot);
    let probe = inner_of(presentation, area, borders);
    let body_rows = usize::from(probe.height.saturating_sub(1));
    let trailing = if capability == CapabilityState::Available {
        truncation_label(body_rows.min(ranked.len()), ranked.len())
            .unwrap_or_else(|| format!("{} processes", ranked.len()))
    } else {
        // The rows are still drawn — a refused counter is a fact about the row — but
        // the panel says once, at the top, why every figure below reads the same
        // (§4, §9.3: show the limitation rather than substituting a number).
        format!(
            "per-process io {} ({})",
            capability.symbol(),
            capability.label()
        )
    };

    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        "TOP DISK I/O",
        Some(trailing.as_str()),
        false,
        borders,
    );
    if inner.is_empty() {
        return;
    }
    let mut lines = vec![io_header(presentation, inner.width)];
    for process in ranked.iter().take(body_rows) {
        lines.push(io_row(process, state, presentation, inner.width));
    }
    if ranked.is_empty() {
        lines.push(muted_line(
            presentation,
            inner.width,
            "no processes visible",
        ));
    }
    write_lines(buffer, inner, &lines);
}

/// The processes ordered by read plus write throughput, busiest first.
///
/// Ordered by I/O regardless of how the process *table* is sorted, exactly as the
/// CPU screen's ranking is ordered by CPU: this panel answers one question, and
/// inheriting an unrelated ordering would make it answer none.
///
/// A process whose counters are unavailable sorts last rather than as zero — §4
/// again, and here it is the difference between "this process is idle" and "the OS
/// would not tell us". Kernel threads are excluded: §7.2 allows hiding them, their
/// per-process counters are refused on both platforms, and the question this panel
/// answers is which *application* is writing.
fn ranked_by_io(snapshot: &SystemSnapshot) -> Vec<&ProcessSnapshot> {
    let mut ranked: Vec<&ProcessSnapshot> = snapshot
        .processes
        .iter()
        .filter(|process| !process.is_kernel_thread)
        .collect();
    ranked.sort_by(|left, right| {
        io_rank(right)
            .total_cmp(&io_rank(left))
            // Cumulative bytes second. On a real machine most processes are idle at
            // any given second, so without this the rows below the busy handful were
            // ordered by PID — thirty rows of `0B/s` in launch order, which answers
            // nothing. Ordered by what each process has actually written since it
            // started, the same rows name the heavy users of the disk.
            .then_with(|| io_total(right).cmp(&io_total(left)))
            .then_with(|| left.identity.pid.cmp(&right.identity.pid))
    });
    ranked
}

/// The primary sort key: bytes per second in both directions, or `-1` for no
/// measurement.
///
/// The sentinel is negative so that an unmeasured process cannot outrank a measured
/// idle one, and a process with only one direction available still ranks on the
/// direction it has.
fn io_rank(process: &ProcessSnapshot) -> f64 {
    let read = process.io.read.fresh().map(|rate| rate.per_second());
    let write = process.io.write.fresh().map(|rate| rate.per_second());
    match (read, write) {
        (None, None) => -1.0,
        (read, write) => read.unwrap_or(0.0) + write.unwrap_or(0.0),
    }
}

/// The secondary sort key: cumulative bytes, where the platform reports them.
///
/// `None` rather than zero for an unavailable counter, so that — [`Option`] ordering
/// putting `None` first — it has to be compared in the same direction as the rate and
/// cannot promote a process whose totals were refused above one that reported some.
fn io_total(process: &ProcessSnapshot) -> Option<u64> {
    let read = process.io.read_total_bytes.fresh();
    let write = process.io.write_total_bytes.fresh();
    match (read, write) {
        (None, None) => None,
        (read, write) => Some(
            read.copied()
                .unwrap_or(0)
                .saturating_add(write.copied().unwrap_or(0)),
        ),
    }
}

/// The top-I/O section's column header.
fn io_header(presentation: Presentation<'_>, width: u16) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    let muted = presentation.style(Token::Muted);
    for (text, cells, align) in [
        ("READ/s", RATE_WIDTH, Align::Right),
        ("WRITE/s", RATE_WIDTH, Align::Right),
        ("TOTAL R", BYTES_WIDTH + 2, Align::Right),
        ("TOTAL W", BYTES_WIDTH + 2, Align::Right),
        ("PID", PID_WIDTH, Align::Right),
    ] {
        row.push_field(text, cells, align, muted);
        row.pad(1);
    }
    let remaining = row.remaining();
    row.push_field("NAME", remaining, Align::Left, muted);
    row.finish()
}

/// One process's I/O row.
///
/// The two `TOTAL` columns are the process's own cumulative counters — bytes since
/// *it* started, which is what `/proc/<pid>/io` and `proc_pidinfo` report — and not
/// an accumulation monitrs performed. A platform that does not expose them says so
/// in the cell rather than showing the rate twice.
fn io_row(
    process: &ProcessSnapshot,
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Line<'static> {
    let units = presentation.units();
    let glyphs = presentation.glyphs();
    let mut row = row_builder(presentation, width);
    for rate in [&process.io.read, &process.io.write] {
        let display = states::describe_byte_rate(rate, units);
        row.push_field(
            &display.fitted(usize::from(RATE_WIDTH), glyphs),
            RATE_WIDTH,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    for total in [&process.io.read_total_bytes, &process.io.write_total_bytes] {
        let display = states::describe_bytes(total, units);
        row.push_field(
            &display.fitted(usize::from(BYTES_WIDTH + 2), glyphs),
            BYTES_WIDTH + 2,
            Align::Right,
            presentation.metric_style(&display),
        );
        row.pad(1);
    }
    row.push_field(
        &process.identity.pid.to_string(),
        PID_WIDTH,
        Align::Right,
        presentation.style(Token::Muted),
    );
    row.pad(1);
    // The selected process is marked here too, so moving the selection on the
    // process screen and switching to this one does not lose track of it (§7.2).
    let selected = state.selected() == Some(process.identity);
    let remaining = row.remaining();
    row.push_field(
        &process.name,
        remaining,
        Align::Left,
        if selected {
            presentation.selection().into_style()
        } else {
            presentation.style(Token::Text)
        },
    );
    row.finish()
}

/// Draws the aggregate throughput history (§7.3's historical graph).
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
    // A byte rate has no natural 100%, so both plots are self-scaling and the panel
    // states the ceiling they are drawn against — in the trailing label, where
    // changing it cannot move the plots (§5.4).
    let peak = plot_peak(ring, HistoryMetric::DiskWrite, units).map_or_else(
        || "all devices".to_owned(),
        |peak| format!("all devices, write peak {peak}"),
    );
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        &title,
        Some(peak.as_str()),
        false,
        borders,
    ));
    if inner.is_empty() {
        return;
    }

    let read = plot_series(ring, HistoryMetric::DiskRead);
    let write = plot_series(ring, HistoryMetric::DiskWrite);
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
        Sparkline::new(presentation, &read)
            .with_label("READ")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .self_scaling(true)
            .with_token(Token::Graph1)
            .render(rect, buffer);
    }
    if let Some(rect) = next_row() {
        Sparkline::new(presentation, &write)
            .with_label("WRITE")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .self_scaling(true)
            .with_token(Token::Graph3)
            .render(rect, buffer);
    }
    // §2.1 and §26: while the timeline is frozen the caret is what makes the panel
    // unmistakably historical, so it takes the next row it can get.
    if let Some(offset) = caret
        && let Some(rect) = next_row()
    {
        SparklineCaret::new(presentation, &write, offset)
            .with_label("WRITE")
            .with_label_width(HISTORY_LABEL_WIDTH)
            .with_note(&note)
            .render(rect, buffer);
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::time::{Instant, SystemTime};

    use monitrs_core::model::{InodeUsage, MetricState, ProcessIdentity, ProcessIo, ProcessState};
    use monitrs_core::units::{Percent, Rate};

    use crate::widgets::MetricDisplay;

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

    /// The busy display for a device, for assertions about the capability gate.
    fn busy_display(device: &DiskSnapshot) -> MetricDisplay {
        states::describe_percent(&device.busy)
    }

    fn filesystem(mount: &str, kind: FilesystemKind) -> FilesystemSnapshot {
        FilesystemSnapshot {
            mount_point: mount.into(),
            device: Some("disk0s1".into()),
            fs_type: Some("ext4".into()),
            total_bytes: 1_000_000_000,
            available_bytes: MetricState::Available(250_000_000),
            used_bytes: MetricState::Available(750_000_000),
            usage: Percent::new(75.0).map_or(MetricState::Unsupported, MetricState::Available),
            inodes: InodeUsage::from_counts(1_000_000, 900_000),
            kind,
            read_only: false,
        }
    }

    fn process(pid: u32, name: &str, io: ProcessIo) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, 42),
            parent_pid: Some(1),
            name: name.into(),
            command: name.into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Sleeping,
            cpu: MetricState::Unsupported,
            memory: monitrs_core::model::ProcessMemory::WARMING_UP,
            io,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    fn io_of(read: f64, write: f64) -> ProcessIo {
        ProcessIo {
            read: Rate::new(read).map_or(MetricState::Unsupported, MetricState::Available),
            write: Rate::new(write).map_or(MetricState::Unsupported, MetricState::Available),
            read_total_bytes: MetricState::Available(1_048_576),
            write_total_bytes: MetricState::Available(2_097_152),
        }
    }

    fn snapshot() -> SystemSnapshot {
        let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
        snapshot.filesystems = vec![
            filesystem("/", FilesystemKind::Physical),
            filesystem("/dev", FilesystemKind::Virtual),
            filesystem("/Volumes/backup", FilesystemKind::Removable),
        ];
        snapshot.disks = vec![DiskSnapshot::warming_up("disk0".into())];
        snapshot
    }

    #[test]
    fn virtual_filesystems_are_hidden_and_the_count_is_disclosed() {
        // §7.3 asks for the filter; a filter the reader cannot see is
        // indistinguishable from a collector that missed the mounts.
        let snapshot = snapshot();
        let (shown, hidden) = partition_filesystems(&snapshot);
        assert_eq!(hidden, 1);
        assert_eq!(shown.len(), 2);
        assert!(shown.iter().all(|fs| !fs.kind.hidden_by_default()));
    }

    #[test]
    fn a_removable_mount_is_shown_and_labelled() {
        // Removable storage is real storage, and which of it is full is the whole
        // question this section answers.
        let removable = filesystem("/Volumes/backup", FilesystemKind::Removable);
        assert_eq!(filesystem_kind_text(&removable), "ext4*");
        let network = filesystem("/mnt/nfs", FilesystemKind::Network);
        assert_eq!(filesystem_kind_text(&network), "ext4@");
        let mut read_only = filesystem("/", FilesystemKind::Physical);
        read_only.read_only = true;
        assert_eq!(filesystem_kind_text(&read_only), "ext4 ro");
    }

    #[test]
    fn device_busy_is_hidden_when_the_capability_says_it_is_not_real() {
        // §7.3: busy/utilization only where semantically correct. The columns are
        // dropped rather than filled with `n/a` on every row.
        let mut snapshot = snapshot();
        snapshot.capabilities.disk_busy = CapabilityState::Unsupported;
        let show = snapshot.capabilities.disk_busy == CapabilityState::Available;
        assert!(!show);
        let header = throughput_header(presentation(), 120, show);
        assert!(!text_of(&header).contains("BUSY"), "{}", text_of(&header));
        assert!(!text_of(&header).contains("QUEUE"), "{}", text_of(&header));

        snapshot.capabilities.disk_busy = CapabilityState::Available;
        let header = throughput_header(presentation(), 120, true);
        assert!(text_of(&header).contains("BUSY"), "{}", text_of(&header));
    }

    #[test]
    fn an_unmeasured_device_busy_is_never_rendered_as_zero() {
        // §4, at the level that matters: the fake platform reports no busy figure,
        // and the cell must not read as an idle device.
        let device = DiskSnapshot::warming_up("disk0".into());
        let display = busy_display(&device);
        assert!(display.is_placeholder());
        let line = throughput_row(presentation(), 140, &device, true);
        assert!(!text_of(&line).contains(" 0%"), "{}", text_of(&line));
    }

    #[test]
    fn capacity_and_throughput_never_share_a_percentage_column() {
        // §7.3, §26: the two are different metrics, and this is the assertion that
        // they are rendered under different headings.
        let capacity = capacity_header(presentation(), 140, 12, true);
        let throughput = throughput_header(presentation(), 140, true);
        assert!(text_of(&capacity).contains("USE%"));
        assert!(!text_of(&capacity).contains("BUSY"));
        assert!(text_of(&throughput).contains("BUSY"));
        assert!(!text_of(&throughput).contains("USE%"));
    }

    #[test]
    fn the_inode_share_is_its_own_column_and_not_folded_into_capacity() {
        // A filesystem can be out of inodes at 40% used, so the two percentages
        // cannot be one column — and each one's heading has to say which it is.
        let header = capacity_header(presentation(), 140, 12, true);
        let text = text_of(&header);
        assert!(text.contains("USE%"), "{text}");
        assert!(text.contains("INODE%"), "{text}");
        assert!(text.contains("IFREE"), "{text}");
    }

    #[test]
    fn a_filesystem_with_no_inode_table_reads_as_unavailable_and_not_as_empty() {
        // §4: `n/a` says "this filesystem has no inode table"; `0%` would say the
        // table is empty, and an empty table is not a thing a filesystem has.
        let mut fs = filesystem("/dev", FilesystemKind::Virtual);
        fs.inodes = MetricState::Unsupported;
        let line = capacity_row(presentation(), 140, &fs, 12, true, false);
        let text = text_of(&line);
        assert!(text.contains("n/a"), "{text}");
        assert!(!text.contains("  0%"), "{text}");
    }

    #[test]
    fn a_refused_inode_read_says_denied_rather_than_n_a() {
        // The two facts are different — one resolves with privileges, the other never
        // will — and the column is deliberately wide enough to keep them apart (§4).
        let mut fs = filesystem("/secret", FilesystemKind::Physical);
        fs.inodes = MetricState::PermissionDenied;
        let text = text_of(&capacity_row(presentation(), 140, &fs, 12, true, false));
        assert!(text.contains("denied"), "{text}");
    }

    #[test]
    fn two_mounts_on_one_device_are_marked_and_a_lone_mount_is_not() {
        // The APFS case: `/` and `/System/Volumes/Data` both report the container's
        // full size, and a reader who adds them gets twice the disk.
        let mut snapshot = snapshot();
        snapshot.filesystems = vec![
            filesystem("/", FilesystemKind::Physical),
            filesystem("/System/Volumes/Data", FilesystemKind::Physical),
        ];
        let (shown, _) = partition_filesystems(&snapshot);
        let shared = shared_devices(&shown);
        assert_eq!(shared.len(), 1, "one device backs both mounts");
        assert!(shown.iter().all(|fs| is_shared(fs, &shared)));

        let mut alone = filesystem("/Volumes/backup", FilesystemKind::Removable);
        alone.device = Some("disk9s1".into());
        assert!(!is_shared(&alone, &shared));
        let marked = text_of(&capacity_row(presentation(), 140, shown[0], 12, true, true));
        assert!(marked.starts_with(SHARED_MARKER), "{marked}");
        let plain = text_of(&capacity_row(presentation(), 140, &alone, 12, true, false));
        assert!(!plain.starts_with(SHARED_MARKER), "{plain}");
    }

    #[test]
    fn mounts_whose_device_is_unknown_are_never_grouped_together() {
        // Two `None` devices are not evidence of one device, and marking them would
        // invent the claim the mark makes.
        let mut left = filesystem("/a", FilesystemKind::Physical);
        let mut right = filesystem("/b", FilesystemKind::Physical);
        left.device = None;
        right.device = None;
        let shown = vec![&left, &right];
        assert!(shared_devices(&shown).is_empty());
    }

    #[test]
    fn a_narrow_panel_keeps_the_disclosures_that_fit_instead_of_losing_them_all() {
        // The panel drops its trailing label whole, so a sentence that overflowed
        // would take the mount count and the hidden-mount disclosure with it. The
        // order is the priority, and a part that does not fit ends the label (§5.4).
        let parts = vec![
            "2 mounted".to_owned(),
            "1 virtual hidden".to_owned(),
            format!("{SHARED_MARKER} shares a device"),
            "SIZE is not additive".to_owned(),
        ];
        assert_eq!(
            fit_label(&parts, CAPACITY_TITLE, 140),
            "2 mounted, 1 virtual hidden, = shares a device, SIZE is not additive"
        );
        assert_eq!(
            fit_label(&parts, CAPACITY_TITLE, 80),
            "2 mounted, 1 virtual hidden, = shares a device",
            "the mark's meaning outranks its consequence, and both outrank nothing"
        );
        assert_eq!(fit_label(&parts, CAPACITY_TITLE, 42), "2 mounted");
        assert_eq!(fit_label(&parts, CAPACITY_TITLE, 20), "");
        // And whatever it produces fits the room the panel will have for it.
        for width in 0..=200u16 {
            let label = fit_label(&parts, CAPACITY_TITLE, width);
            assert!(
                display_width(&label) + display_width(CAPACITY_TITLE) + 10
                    <= usize::from(width).max(display_width(CAPACITY_TITLE) + 10),
                "width {width} produced {label:?}"
            );
        }
    }

    #[test]
    fn a_narrow_row_drops_the_capacity_meter_rather_than_the_numbers() {
        // The numbers are the measurement; the bar is a reading aid.
        assert_eq!(capacity_meter_width(60, true), 0);
        assert!(capacity_meter_width(160, true) >= MIN_CAPACITY_METER);
        // And the inode columns free their cells for the meter when they are dropped,
        // at a width where the meter's own cap is not what decides it.
        assert!(capacity_meter_width(95, false) > capacity_meter_width(95, true));
    }

    #[test]
    fn an_unmapped_device_says_so_rather_than_looking_unmounted() {
        // §8.6 puts the mapping in the on-demand tier, so an empty list is "not
        // known yet" and not "mounted nowhere".
        let device = DiskSnapshot::warming_up("disk0".into());
        let line = throughput_row(presentation(), 160, &device, false);
        assert!(text_of(&line).contains("not mapped"), "{}", text_of(&line));
    }

    #[test]
    fn idle_processes_are_ordered_by_what_they_have_written_and_not_by_pid() {
        // On a real machine almost everything is idle in any given second. Ordered by
        // PID those rows say nothing; ordered by cumulative bytes they name the heavy
        // users of the disk.
        let mut snapshot = snapshot();
        let mut small = process(700, "small", io_of(0.0, 0.0));
        small.io.read_total_bytes = MetricState::Available(1_024);
        small.io.write_total_bytes = MetricState::Available(0);
        let mut large = process(900, "large", io_of(0.0, 0.0));
        large.io.read_total_bytes = MetricState::Available(4_294_967_296);
        large.io.write_total_bytes = MetricState::Available(1_073_741_824);
        snapshot.processes = vec![small, large];

        let ranked = ranked_by_io(&snapshot);
        assert_eq!(
            &*ranked[0].name, "large",
            "the higher PID with the larger total must come first"
        );
    }

    #[test]
    fn a_refused_total_never_outranks_a_reported_one() {
        // §4 again, on the secondary key: `None` is not a large number and not a
        // small one, and it must not decide the order in its favour.
        let mut snapshot = snapshot();
        let mut denied = process(10, "denied-totals", io_of(0.0, 0.0));
        denied.io.read_total_bytes = MetricState::PermissionDenied;
        denied.io.write_total_bytes = MetricState::PermissionDenied;
        snapshot.processes = vec![denied, process(11, "measured", io_of(0.0, 0.0))];
        let ranked = ranked_by_io(&snapshot);
        assert_eq!(&*ranked[0].name, "measured");
        assert_eq!(io_total(ranked[1]), None);
    }

    #[test]
    fn the_io_ranking_is_by_read_plus_write_and_ignores_the_table_sort() {
        let mut snapshot = snapshot();
        snapshot.processes = vec![
            process(10, "quiet", io_of(1_000.0, 1_000.0)),
            process(11, "writer", io_of(0.0, 90_000.0)),
            process(12, "reader", io_of(50_000.0, 0.0)),
        ];
        let ranked = ranked_by_io(&snapshot);
        let names: Vec<&str> = ranked.iter().map(|process| &*process.name).collect();
        assert_eq!(names, vec!["writer", "reader", "quiet"]);
    }

    #[test]
    fn a_process_whose_counters_were_refused_sorts_below_a_measured_idle_one() {
        // §4: "denied" is not "zero", and a denied row must not push a measured row
        // off the panel by pretending to be busier than it.
        let mut snapshot = snapshot();
        snapshot.processes = vec![
            process(20, "denied", ProcessIo::UNSUPPORTED),
            process(21, "idle", io_of(0.0, 0.0)),
        ];
        let ranked = ranked_by_io(&snapshot);
        assert_eq!(&*ranked[0].name, "idle");
        assert_eq!(&*ranked[1].name, "denied");
        assert!(io_rank(ranked[1]) < 0.0);
    }

    #[test]
    fn a_kernel_thread_is_not_ranked_as_an_application() {
        // §7.2 allows hiding them, and "which application is writing" is the question
        // this panel exists to answer.
        let mut snapshot = snapshot();
        let mut thread = process(30, "kworker/2:1", io_of(99_000_000.0, 99_000_000.0));
        thread.is_kernel_thread = true;
        snapshot.processes = vec![thread, process(31, "rustc", io_of(1.0, 1.0))];
        let ranked = ranked_by_io(&snapshot);
        assert_eq!(ranked.len(), 1);
        assert_eq!(&*ranked[0].name, "rustc");
    }

    #[test]
    fn a_refused_io_row_is_still_drawn_and_still_says_why() {
        // The row is information: it names a process whose counters we cannot see.
        // What it must never contain is a zero.
        let denied = process(40, "launchd", ProcessIo::UNSUPPORTED);
        let text = text_of(&io_row(&denied, &AppState::default(), presentation(), 140));
        assert!(text.contains("launchd"), "{text}");
        assert!(text.contains("n/a"), "{text}");
        assert!(!text.contains("0B/s"), "{text}");
    }

    #[test]
    fn the_io_totals_are_a_separate_column_from_the_rates() {
        // §7.4's "two totals, never merged" reasoning applies here too: a cumulative
        // byte count and a rate are different figures and cannot share a heading.
        let header = text_of(&io_header(presentation(), 140));
        assert!(header.contains("READ/s"), "{header}");
        assert!(header.contains("TOTAL R"), "{header}");
        assert!(header.contains("PID"), "{header}");
        assert!(header.contains("NAME"), "{header}");
    }

    #[test]
    fn a_compact_count_stays_inside_the_column_and_keeps_small_counts_exact() {
        assert_eq!(format_count_compact(0), "0");
        assert_eq!(format_count_compact(9_999), "9999");
        assert_eq!(format_count_compact(10_000), "10K");
        assert_eq!(format_count_compact(482_000), "482K");
        assert_eq!(format_count_compact(4_398_046), "4.4M");
        assert_eq!(format_count_compact(4_882_812_499), "4.9G");
        for count in [0u64, 1, 9_999, 10_000, 12_345_678, 999_999_999_999_999] {
            assert!(
                display_width(&format_count_compact(count)) <= usize::from(INODE_FREE_WIDTH),
                "{count} did not fit"
            );
        }
    }

    #[test]
    fn every_panel_gets_rows_on_a_full_height_terminal_and_none_panics_when_tiny() {
        // The complaint this screen is answering: on a 48-row terminal the two upper
        // tables need nine rows between them, and the rest belonged to something.
        let body = 34u16;
        let capacity = panel_height(2).min(half_of(body));
        let devices = panel_height(1).min(third_of(body));
        let rest = body - capacity - devices;
        assert!(rest >= HISTORY_HEIGHT + MIN_TOP_IO_ROWS);
        assert!(
            rest - HISTORY_HEIGHT >= 12,
            "the ranking got only {} rows",
            rest - HISTORY_HEIGHT
        );

        // And a terminal too short for four panels drops the optional ones instead of
        // underflowing (§5.7).
        for height in 0..12u16 {
            let capacity = panel_height(2).min(half_of(height)).min(height);
            let remaining = height.saturating_sub(capacity);
            let devices = panel_height(1).min(third_of(height)).min(remaining);
            assert!(capacity + devices <= height.max(3));
        }
    }

    #[test]
    fn the_history_panel_names_the_scale_it_is_drawn_against() {
        // A self-scaling plot without a stated ceiling is not a measurement (§5.4),
        // and this panel's ceiling is a byte rate with no natural 100%.
        let state = AppState::default();
        let units = presentation().units();
        let label = plot_peak(state.history(), HistoryMetric::DiskWrite, units);
        // An empty ring has no peak to name, and the label still says what the plot
        // covers rather than leaving the reader to assume it is one device.
        assert!(label.is_none());
    }

    #[test]
    fn a_stale_inode_count_is_marked_stale_rather_than_shown_as_current() {
        let mut fs = filesystem("/", FilesystemKind::Physical);
        fs.inodes = fs.inodes.into_stale(Duration::from_secs(30));
        let display = states::describe_percent(&fs.inode_usage());
        assert_eq!(display.symbol(), '~');
        assert!(display.age().is_some());
    }
}
