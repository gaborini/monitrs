//! The Storage screen (§7.3): filesystem capacity and device throughput, as two
//! separate and separately labelled sections.
//!
//! ```text
//! + FILESYSTEM CAPACITY --------------------- 2 shown, 1 virtual hidden -+
//! | MOUNT   DEVICE   TYPE    SIZE   USED  AVAIL  USE%  [########----]    |
//! | /       disk0s1  fakefs  494G   374G   120G   76%  [#########---]    |
//! + DEVICE THROUGHPUT ---------------------------------- busy n/a ------+
//! | DEVICE  MODEL      READ/s  WRITE/s   R-IOPS   W-IOPS  MOUNTS         |
//! | disk0   Fake NVMe   18M/s    42M/s      320      810  /              |
//! ```
//!
//! # Why two sections and never one number
//!
//! §7.3 and §26 both say it outright: filesystem capacity and device utilization
//! are different metrics. A filesystem that is 95% full is not busy, and a device
//! saturated at 100% utilization may sit on a nearly empty one. `monitrs-core`
//! keeps them in two types with no overlapping fields, and this screen keeps them
//! in two panels with two headings — so there is no code path that could render
//! "76%" without saying 76% *of what*.
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

use monitrs_core::model::{
    CapabilityState, DiskSnapshot, FilesystemKind, FilesystemSnapshot, SystemSnapshot,
};
use monitrs_core::units::{MAX_BYTE_RATE_WIDTH, MAX_COMPACT_BYTES_WIDTH, format_bytes_compact};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Borders;

use crate::app::AppState;
use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::Presentation;
use crate::widgets::states;

use super::{
    Chrome, SHARED_BOTTOM, draw_bordered_panel, inner_of, muted_line, row_builder, split_rows,
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

/// The narrowest capacity meter worth drawing beside a filesystem row.
const MIN_CAPACITY_METER: u16 = 8;

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

    // Capacity gets the larger share: it is the section with one row per mount,
    // and a machine usually has more mounts than block devices.
    let devices = u16::try_from(snapshot.disks.len().saturating_add(3)).unwrap_or(u16::MAX);
    let device_rows = devices.min(body.height / 2).max(3).min(body.height);
    let rows = split_rows(
        body,
        &[body.height.saturating_sub(device_rows), device_rows],
    );
    if let Some(capacity) = rows.first() {
        // The capacity panel's bottom edge is the throughput panel's top rule
        // (§5.5's shared borders), which is also what keeps the two headings
        // adjacent enough to read as a contrast rather than as two screens.
        draw_capacity(buffer, *capacity, snapshot, presentation, SHARED_BOTTOM);
    }
    if let Some(throughput) = rows.get(1) {
        draw_throughput(buffer, *throughput, snapshot, presentation, Borders::ALL);
    }
}

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
    let mut trailing = format!("{} mounted", shown.len());
    if hidden > 0 {
        // A filter the reader cannot see is indistinguishable from a collector that
        // missed the mounts (§7.3's filtering must be disclosed).
        trailing.push_str(&format!(", {hidden} virtual hidden"));
    }
    if let Some(cut) = truncation_label(body_rows.min(shown.len()), shown.len()) {
        trailing = format!("{cut} mounted, {hidden} virtual hidden");
    }

    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        "FILESYSTEM CAPACITY",
        Some(trailing.as_str()),
        false,
        borders,
    );
    if inner.is_empty() {
        return;
    }
    let meter_width = capacity_meter_width(inner.width);
    let mut lines = vec![capacity_header(presentation, inner.width, meter_width)];
    for filesystem in shown.iter().take(body_rows) {
        lines.push(capacity_row(
            presentation,
            inner.width,
            filesystem,
            meter_width,
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

/// The mounts to show, and how many were hidden by the §7.3 filter.
fn partition_filesystems(snapshot: &SystemSnapshot) -> (Vec<&FilesystemSnapshot>, usize) {
    let (hidden, shown): (Vec<_>, Vec<_>) = snapshot
        .filesystems
        .iter()
        .partition(|filesystem| filesystem.kind.hidden_by_default());
    (shown, hidden.len())
}

/// Cells the capacity meter gets, or zero when the row is too narrow for one.
fn capacity_meter_width(width: u16) -> u16 {
    let fixed = MOUNT_WIDTH + DEVICE_WIDTH + FS_TYPE_WIDTH + BYTES_WIDTH * 3 + PERCENT_WIDTH + 8;
    let spare = width.saturating_sub(fixed);
    if spare >= MIN_CAPACITY_METER {
        spare.min(24)
    } else {
        0
    }
}

/// The capacity section's column header.
fn capacity_header(presentation: Presentation<'_>, width: u16, meter: u16) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    let muted = presentation.style(Token::Muted);
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
) -> Line<'static> {
    let units = presentation.units();
    let glyphs = presentation.glyphs();
    let mut row = row_builder(presentation, width);
    let text = presentation.style(Token::Text);
    let muted = presentation.style(Token::Muted);

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

#[cfg(test)]
mod tests {
    use std::time::{Instant, SystemTime};

    use monitrs_core::model::MetricState;
    use monitrs_core::units::Percent;

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
            kind,
            read_only: false,
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
        let text: String = header
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains("BUSY"), "{text}");
        assert!(!text.contains("QUEUE"), "{text}");

        snapshot.capabilities.disk_busy = CapabilityState::Available;
        let header = throughput_header(presentation(), 120, true);
        let text: String = header
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("BUSY"), "{text}");
    }

    #[test]
    fn an_unmeasured_device_busy_is_never_rendered_as_zero() {
        // §4, at the level that matters: the fake platform reports no busy figure,
        // and the cell must not read as an idle device.
        let device = DiskSnapshot::warming_up("disk0".into());
        let display = busy_display(&device);
        assert!(display.is_placeholder());
        let line = throughput_row(presentation(), 140, &device, true);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains(" 0%"), "{text}");
    }

    #[test]
    fn capacity_and_throughput_never_share_a_percentage_column() {
        // §7.3, §26: the two are different metrics, and this is the assertion that
        // they are rendered under different headings.
        let capacity = capacity_header(presentation(), 120, 12);
        let throughput = throughput_header(presentation(), 120, true);
        let text = |line: &Line<'static>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert!(text(&capacity).contains("USE%"));
        assert!(!text(&capacity).contains("BUSY"));
        assert!(text(&throughput).contains("BUSY"));
        assert!(!text(&throughput).contains("USE%"));
    }

    #[test]
    fn a_narrow_row_drops_the_capacity_meter_rather_than_the_numbers() {
        // The numbers are the measurement; the bar is a reading aid.
        assert_eq!(capacity_meter_width(60), 0);
        assert!(capacity_meter_width(160) >= MIN_CAPACITY_METER);
    }

    #[test]
    fn an_unmapped_device_says_so_rather_than_looking_unmounted() {
        // §8.6 puts the mapping in the on-demand tier, so an empty list is "not
        // known yet" and not "mounted nowhere".
        let device = DiskSnapshot::warming_up("disk0".into());
        let line = throughput_row(presentation(), 160, &device, false);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("not mapped"), "{text}");
    }
}
