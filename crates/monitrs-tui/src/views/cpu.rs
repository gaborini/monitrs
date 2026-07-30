//! The CPU screen: one row per logical CPU, grouped by what kind of core it is.
//!
//! ```text
//! + CPU ------------------------- 12 logical, 12 physical  no frequency -+
//! | TOTAL  36%  [#############-------------------]  user 21%  sys 15%    |
//! | LOAD   4.26  2.98  2.11    over 12 cores: 0.36 per core              |
//! + PERFORMANCE ------------------------------- 8 logical, 8 physical ---+
//! |  0   82%  [#########################-------]  user 61%  sys 21%     |
//! |  1   74%  [######################----------]  user 55%  sys 19%     |
//! + EFFICIENCY -------------------------------- 4 logical, 4 physical ---+
//! |  8   12%  [####----------------------------]  user  9%  sys  3%     |
//! + BUSIEST PROCESSES ---------------------------------------------------+
//! |    CPU%  PID    NAME                                                |
//! |     96%  45241  Cursor Helper (Renderer)                            |
//! ```
//!
//! # Why the grouping is the point
//!
//! A flat list of twelve numbers is what the Overview's core strip already gives, and
//! it is not what a per-core screen is *for*. Apple Silicon splits its CPUs into
//! performance and efficiency cores, and the same eight numbers mean opposite things
//! depending on which is which: four efficiency cores at 90% with the performance cores
//! idle is a machine doing almost nothing, while the reverse is a machine working hard.
//! [`CoreClass`] carries the platform's own names — `hw.perflevelN.name` on macOS —
//! rather than a vocabulary invented here, and a homogeneous machine reports no classes
//! at all, in which case this screen draws one unlabelled group.
//!
//! # What it does not invent
//!
//! Per-core *history* is not drawn. The history ring keeps one aggregate CPU series
//! (§8.5's `HistoryMetric::CpuBusy`), not one per core, and a sparkline per core would
//! either need twelve more rings or would have to fabricate a series from the single
//! sample this frame holds. §4 does not allow the second and §16.1's memory budget does
//! not invite the first without a measurement first.
//!
//! Frequency is shown where the platform reports it and named as absent where it does
//! not — Apple Silicon exposes no public per-core clock, so on this machine the header
//! says so rather than leaving a gap a reader would fill with a guess.

use monitrs_core::model::{CoreClass, CpuUsage, MetricState, ProcessSnapshot, SystemSnapshot};
use monitrs_core::units::Percent;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Borders;

use crate::app::AppState;
use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::{Meter, Presentation, states};

use super::{Chrome, draw_bordered_panel, inset, muted_line, row_builder, split_rows, write_lines};

/// Cells reserved for a core's index, which fits a 4096-CPU machine.
const INDEX_WIDTH: u16 = 4;
/// Cells reserved for a percentage, matching the process table's `CPU%` column.
const PERCENT_WIDTH: u16 = 5;
/// The most core rows drawn before the panel says how many it left out.
///
/// §7.1 says a machine with too many cores should be aggregated rather than rendered as
/// hundreds of rows. This screen's answer is to draw what fits and name the remainder,
/// because a per-core screen that silently showed 32 of 128 cores would be worse than
/// one that admits it.
const MAX_CORE_ROWS: usize = 128;

/// Draws the CPU screen.
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

    // Three sections: the aggregate, the cores, and the processes responsible.
    //
    // The cores take exactly what they need and the processes take the rest, rather than
    // the other way round. Sizing the core panels by share of the screen left a dozen
    // blank rows on a twelve-core machine while the process list showed five entries —
    // the same emptiness that made the Storage screen worth complaining about. A machine
    // with more cores than fit reverses the pressure, and `draw_core_group` says how many
    // it left out.
    let summary_rows = 4u16;
    let groups = group_count(snapshot);
    let core_rows = core_panel_height(snapshot, groups)
        .min(body.height.saturating_sub(summary_rows).saturating_sub(4));
    let process_rows = body
        .height
        .saturating_sub(summary_rows)
        .saturating_sub(core_rows);
    let rows = split_rows(body, &[summary_rows, core_rows, process_rows]);

    if let Some(area) = rows.first() {
        draw_summary(buffer, *area, snapshot, presentation);
    }
    if let Some(area) = rows.get(1) {
        draw_cores(buffer, *area, snapshot, presentation);
    }
    if let Some(area) = rows.get(2) {
        draw_busiest(buffer, *area, state, snapshot, presentation);
    }
}

/// How many core groups this machine will draw.
fn group_count(snapshot: &SystemSnapshot) -> usize {
    snapshot.cpu.per_core.fresh().map_or(1, |cores| {
        grouped(&snapshot.cpu.core_classes, cores.len()).len()
    })
}

/// The height the core panels want: one row per core plus a border per group.
fn core_panel_height(snapshot: &SystemSnapshot, groups: usize) -> u16 {
    let cores = snapshot
        .cpu
        .per_core
        .fresh()
        .map_or(usize::from(snapshot.cpu.logical_count), Vec::len)
        .min(MAX_CORE_ROWS);
    let borders = groups.saturating_mul(2);
    u16::try_from(cores.saturating_add(borders)).unwrap_or(u16::MAX)
}

/// The aggregate: total utilization, its breakdown, and the load average.
fn draw_summary(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
) {
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        "CPU",
        Some(&topology_label(snapshot)),
        false,
        Borders::ALL,
    ));
    if inner.is_empty() {
        return;
    }

    let mut lines = Vec::new();
    let busy = snapshot.cpu.total.as_ref().map(|usage| usage.busy);
    lines.push(
        Meter::new(presentation, busy)
            .with_label("TOTAL")
            .with_note(&breakdown_note(&snapshot.cpu.total))
            .styled_line(inner.width),
    );
    lines.push(load_line(snapshot, presentation, inner.width));
    write_lines(buffer, inner, &lines);
}

/// `12 logical, 12 physical  no frequency` — what kind of machine this is.
///
/// A cgroup ceiling, where one applies, goes here rather than being left to the Inspect
/// screen: this is the header of the screen about CPUs, and the number of CPUs a process
/// may actually use is part of what kind of machine this is. Without it the panels below
/// show twelve cores to a process that can occupy one and a half of them.
fn topology_label(snapshot: &SystemSnapshot) -> String {
    let mut parts = vec![format!("{} logical", snapshot.cpu.logical_count)];
    match snapshot.cpu.physical_count.fresh() {
        Some(count) => parts.push(format!("{count} physical")),
        None => parts.push("physical n/a".to_owned()),
    }
    if snapshot.cpu.is_cpu_limited() {
        parts.push(format!("cgroup {:.1} CPUs", snapshot.cpu.effective_cores()));
    }
    match snapshot.cpu.frequency_mhz.fresh() {
        Some(mhz) => parts.push(format!("{mhz} MHz")),
        // Named rather than omitted: a missing figure a reader cannot see is one they
        // will assume was measured (§4).
        None => parts.push("no frequency".to_owned()),
    }
    parts.join("  ")
}

/// `user 21%  sys 15%  idle 64%` for whatever the platform splits out.
///
/// Only the states this platform reports: §4 forbids printing a zero for a field the
/// OS does not expose, and a `CpuBreakdown` marks each optional field individually.
fn breakdown_note(total: &MetricState<CpuUsage>) -> String {
    let Some(usage) = total.fresh() else {
        return String::new();
    };
    let Some(breakdown) = usage.breakdown.fresh() else {
        return String::new();
    };
    let mut parts = vec![
        format!("user {}", breakdown.user),
        format!("sys {}", breakdown.system),
    ];
    if breakdown.nice.value() > 0.0 {
        parts.push(format!("nice {}", breakdown.nice));
    }
    for (label, value) in [
        ("iowait", &breakdown.iowait),
        ("irq", &breakdown.irq),
        ("steal", &breakdown.steal),
    ] {
        if let Some(percent) = value.fresh().filter(|percent| percent.value() > 0.0) {
            parts.push(format!("{label} {percent}"));
        }
    }
    parts.join("  ")
}

/// `LOAD  4.26  2.98  2.11    over 12 cores: 0.36 per core`
///
/// The per-core figure is the one that answers "is 4.26 a lot", which a bare load
/// average never does — it means something different on 4 cores and on 128.
///
/// # Why a cgroup quota does *not* become the divisor
///
/// It would be the obvious thing to do and it would be wrong. `/proc/loadavg` is not
/// namespaced: inside a container it reports the **host's** run queue, including every
/// process in every other container on the machine. Dividing that by this group's 1.5-CPU
/// quota pairs a host numerator with a container denominator and produces a figure that
/// describes nothing — the same category error §9.2 forbids for memory, where
/// `/proc/meminfo`'s host `used` must never be divided by a cgroup limit.
///
/// So the divisor stays the host's CPU count, and where a quota exists the label says
/// `host cores` to make clear that this figure is about the machine rather than about the
/// container. The group's own saturation is a different measurement — cgroup PSI — and
/// inventing it by division here would be worse than not having it.
fn load_line(
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
    width: u16,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    row.push_field(
        "LOAD",
        INDEX_WIDTH + 2,
        Align::Left,
        presentation.style(Token::Text),
    );
    match snapshot.load.fresh() {
        Some(load) => {
            for value in [load.one, load.five, load.fifteen] {
                row.push_field(
                    &format!("{value:.2}"),
                    7,
                    Align::Right,
                    presentation.style(Token::Text),
                );
            }
            row.pad(4);
            let cores = f32::from(snapshot.cpu.logical_count.max(1));
            // `host cores` only inside a cgroup: on a bare machine there is no other
            // kind of core for the reader to confuse these with, and the word would be
            // noise on every screen to clarify one.
            let scope = if snapshot.cpu.is_cpu_limited() {
                "host cores"
            } else {
                "cores"
            };
            row.push(
                &format!(
                    "over {} {scope}: {:.2} per core",
                    snapshot.cpu.logical_count,
                    load.one / cores
                ),
                presentation.style(Token::Muted),
            );
        }
        None => {
            let display = states::describe_display(&snapshot.load.as_ref().map(|_| 0.0));
            row.push(display.text(), presentation.metric_style(&display));
        }
    }
    row.finish()
}

/// One panel per core class, or one unlabelled panel on a homogeneous machine.
fn draw_cores(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
) {
    let Some(cores) = snapshot.cpu.per_core.fresh() else {
        let inner = inset(draw_bordered_panel(
            buffer,
            area,
            presentation,
            "CORES",
            None,
            false,
            Borders::ALL,
        ));
        let display =
            states::describe_percent(&snapshot.cpu.per_core.as_ref().map(|_| Percent::ZERO));
        write_lines(
            buffer,
            inner,
            &[muted_line(presentation, inner.width, display.text())],
        );
        return;
    };

    let groups = grouped(&snapshot.cpu.core_classes, cores.len());
    // Each group asks for one row per core plus its two borders. Where the total does
    // not fit, the shortfall is taken from the largest group first, so a machine with one
    // huge class and one small one keeps the small one whole rather than truncating both.
    let mut heights: Vec<u16> = groups
        .iter()
        .map(|(_, indices)| u16::try_from(indices.len().saturating_add(2)).unwrap_or(u16::MAX))
        .collect();
    let mut over = heights
        .iter()
        .copied()
        .sum::<u16>()
        .saturating_sub(area.height);
    while over > 0 {
        let Some((index, _)) = heights
            .iter()
            .enumerate()
            .max_by_key(|(_, height)| **height)
            .filter(|(_, height)| **height > 3)
        else {
            break;
        };
        if let Some(height) = heights.get_mut(index) {
            *height -= 1;
            over -= 1;
        }
    }

    for (rect, (name, indices)) in split_rows(area, &heights).into_iter().zip(groups) {
        draw_core_group(buffer, rect, &name, &indices, cores, presentation);
    }
}

/// The core classes as `(name, indices)`, or one unnamed group covering every core.
fn grouped(classes: &[CoreClass], cores: usize) -> Vec<(String, Vec<usize>)> {
    let usable: Vec<(String, Vec<usize>)> = classes
        .iter()
        .map(|class| {
            (
                class.name.to_uppercase(),
                class
                    .logical
                    .iter()
                    .map(|index| usize::from(*index))
                    .filter(|index| *index < cores)
                    .collect::<Vec<usize>>(),
            )
        })
        .filter(|(_, indices)| !indices.is_empty())
        .collect();
    if usable.is_empty() {
        // A homogeneous machine, or a platform that does not classify: one group, and
        // no invented label.
        return vec![("CORES".to_owned(), (0..cores).collect())];
    }
    usable
}

/// One class of cores, one row each.
fn draw_core_group(
    buffer: &mut Buffer,
    area: Rect,
    name: &str,
    indices: &[usize],
    cores: &[CpuUsage],
    presentation: Presentation<'_>,
) {
    let physical = indices.len();
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        name,
        Some(&format!("{physical} logical")),
        false,
        Borders::ALL,
    ));
    if inner.is_empty() {
        return;
    }

    let room = usize::from(inner.height).min(MAX_CORE_ROWS);
    let mut lines: Vec<Line<'static>> = indices
        .iter()
        .take(room)
        .filter_map(|index| cores.get(*index).map(|usage| (index, usage)))
        .map(|(index, usage)| core_line(*index, usage, presentation, inner.width))
        .collect();
    // §7.1: say what was left out rather than showing a subset as if it were all.
    if indices.len() > lines.len() && !lines.is_empty() {
        let hidden = indices.len() - lines.len();
        if let Some(last) = lines.last_mut() {
            *last = muted_line(
                presentation,
                inner.width,
                &format!("{hidden} more cores need a taller terminal"),
            );
        }
    }
    write_lines(buffer, inner, &lines);
}

/// `  0   82%  [####----]  user 61%  sys 21%`
fn core_line(
    index: usize,
    usage: &CpuUsage,
    presentation: Presentation<'_>,
    width: u16,
) -> Line<'static> {
    let note = breakdown_note(&MetricState::Available(*usage));
    Meter::new(presentation, MetricState::Available(usage.busy))
        .with_label(&index.to_string())
        .with_label_width(INDEX_WIDTH)
        .with_note(&note)
        .styled_line(width)
}

/// The processes accounting for the load, which is the question a core view raises.
fn draw_busiest(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    snapshot: &SystemSnapshot,
    presentation: Presentation<'_>,
) {
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        "BUSIEST PROCESSES",
        Some(&format!("{} total", snapshot.process_count())),
        false,
        Borders::ALL,
    ));
    if inner.is_empty() {
        return;
    }

    // Ordered by CPU regardless of how the *table* is sorted: this panel answers one
    // question, and inheriting an unrelated ordering would make it answer none.
    let mut busiest: Vec<&ProcessSnapshot> = snapshot
        .processes
        .iter()
        .filter(|process| !process.is_kernel_thread)
        .collect();
    busiest.sort_by(|left, right| {
        let value = |process: &ProcessSnapshot| process.cpu.fresh().map_or(-1.0, |cpu| cpu.value());
        value(right)
            .total_cmp(&value(left))
            .then_with(|| left.identity.pid.cmp(&right.identity.pid))
    });

    let mut lines = vec![header_line(presentation, inner.width)];
    let room = usize::from(inner.height).saturating_sub(1);
    for process in busiest.into_iter().take(room) {
        lines.push(process_line(process, state, presentation, inner.width));
    }
    write_lines(buffer, inner, &lines);
}

fn header_line(presentation: Presentation<'_>, width: u16) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    for (text, cells, align) in [
        ("CPU%", PERCENT_WIDTH, Align::Right),
        ("PID", 8, Align::Right),
        (
            "NAME",
            width.saturating_sub(PERCENT_WIDTH + 10),
            Align::Left,
        ),
    ] {
        row.push_field(text, cells, align, presentation.style(Token::Muted));
        row.pad(1);
    }
    row.finish()
}

fn process_line(
    process: &ProcessSnapshot,
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    let cpu = states::describe_percent(&process.cpu);
    row.push_field(
        &cpu.fitted(usize::from(PERCENT_WIDTH), presentation.glyphs()),
        PERCENT_WIDTH,
        Align::Right,
        presentation.metric_style(&cpu),
    );
    row.pad(1);
    row.push_field(
        &process.identity.pid.to_string(),
        8,
        Align::Right,
        presentation.style(Token::Muted),
    );
    row.pad(1);
    // The selected process is marked here too, so moving the selection on the process
    // screen and switching to this one does not lose track of it (§7.2).
    let selected = state.selected() == Some(process.identity);
    row.push_field(
        &process.name,
        width.saturating_sub(PERCENT_WIDTH + 10),
        Align::Left,
        if selected {
            presentation.selection().into_style()
        } else {
            presentation.style(Token::Text)
        },
    );
    row.finish()
}
