//! The Inspect screen (§7.5): the detailed explanation surface.
//!
//! ```text
//! + SYSTEM ---------------------------+ DIAGNOSTICS ------------------+
//! |  os          fake-os 1.0          |PRESSURE                      |
//! |  kernel      fake-kernel 1.0      |X MEM  critical          1.2G |
//! |  memory      32 GiB, swap 2.0 GiB |  available < 15% of total    |
//! + SELECTED PROCESS -----------------+UNAVAILABLE METRICS           |
//! |  identity    31842 (start 900100) |  - disk busy   unsupported   |
//! |  command     cargo build --release|  ! process I/O permission .. |
//! ```
//!
//! # Three subsections, and why they are in this order
//!
//! §7.5 lists the system, the selected process, and the diagnostics. The
//! diagnostics get their own column on a wide terminal because they are the
//! section that grows: nine pressure signals, twenty-two capabilities, four
//! collector tiers, and an unbounded-in-principle issue list. The two left-hand
//! sections are fixed-length descriptions of the machine and of one process.
//!
//! # Environment variables are absent, not hidden
//!
//! §7.5 forbids showing environment-variable values and §15.2 forbids logging
//! them. [`ProcessDetail`] has no field for them, so this screen cannot render
//! them even by mistake — and no field is added here to make it possible.
//!
//! # Nothing on this screen is a conclusion
//!
//! Every figure is either a measurement or the rule that produced a state. The
//! container/VM classification is rendered with its evidence and its confidence
//! because §7.5 requires it to be *clearly labelled heuristic*, and the pressure
//! rows carry the rule text because §2.3 requires the derivation to be visible.
//! No line on this screen says what the user should do.
//!
//! [`ProcessDetail`]: monitrs_core::model::ProcessDetail

use core::time::Duration;

use monitrs_core::model::{
    AncestorEntry, CapabilityState, CollectorHealth, HostEnvironment, MetricState, PressureState,
    ProcessDetail, ProcessIdentity, ProcessSnapshot, SystemSnapshot, Tier,
};
use monitrs_core::units::{
    ByteUnits, Percent, format_age, format_bytes, format_bytes_compact, format_duration,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Borders;

use crate::app::{AppState, FRAME_BUDGET, RenderTiming};
use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Presentation, RadarRow};

use super::{
    Chrome, SHARED_BOTTOM, draw_bordered_panel, inner_of, muted_line, row_builder, split_columns,
    split_rows, truncation_label, wall_clock_of, write_lines,
};

/// Cells reserved for a fact's label, so the values line up down the panel.
const LABEL_WIDTH: u16 = 13;

/// Cells a sub-block's entries are indented by, so headings stand out without
/// colour (§5.2).
const INDENT: u16 = 2;

/// The narrowest terminal that gets the diagnostics in their own column.
const TWO_COLUMN_WIDTH: u16 = 100;

/// The share of a two-column layout the left column takes.
const LEFT_COLUMN_DIVISOR: u16 = 2;

/// The fewest rows a section is given, frame included.
const MIN_SECTION_ROWS: u16 = 4;

/// Cells reserved for a capability or metric name in the diagnostics blocks.
const NAME_WIDTH: u16 = 24;

/// Cells reserved for the state word in a diagnostics pressure row.
///
/// Wider than [`crate::widgets::radar::STATE_WIDTH`] on purpose. This is the
/// explanation surface: it has the room for `permission denied` and `link speed
/// unknown` in full, and the eight-cell radar column degrades both to `n/a` — which
/// reads as `unsupported` and is exactly the confusion §2.3's explicit unavailable
/// state exists to prevent. The value is the width of `needs a second sample`, the
/// longest §4 placeholder there is.
const DIAGNOSTIC_STATE_WIDTH: u16 = 21;

/// Borders for the top-left panel of the two-column layout.
const SHARED_BOTTOM_AND_RIGHT: Borders = Borders::TOP.union(Borders::LEFT);

/// Borders for the bottom-left panel of the two-column layout.
const SHARED_RIGHT_ONLY: Borders = Borders::ALL.difference(Borders::RIGHT);

/// Draws the Inspect screen (§7.5).
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, presentation: Presentation<'_>) {
    let Some(body) = Chrome::resolve(area).body else {
        return;
    };
    let buffer = frame.buffer_mut();

    if body.width >= TWO_COLUMN_WIDTH {
        let (left, right) = split_columns(body, body.width / LEFT_COLUMN_DIVISOR);
        // The system block carries the collector, render, and overhead figures §7.5
        // puts in that subsection, so it is the taller of the two on the left; the
        // process block describes one process and is bounded.
        let process_height = (left.height / 3).max(MIN_SECTION_ROWS).min(left.height);
        let rows = split_rows(
            left,
            &[left.height.saturating_sub(process_height), process_height],
        );
        if let Some(area) = rows.first() {
            // Two vertically stacked panels share the row between them (§5.5), and
            // the left column lends its right border to the diagnostics column.
            draw_system(buffer, *area, state, presentation, SHARED_BOTTOM_AND_RIGHT);
        }
        if let Some(area) = rows.get(1) {
            draw_process(buffer, *area, state, presentation, SHARED_RIGHT_ONLY);
        }
        draw_diagnostics(buffer, right, state, presentation, Borders::ALL);
        return;
    }

    // One column: the three sections share the height in the order §7.5 lists them.
    let third = (body.height / 3).max(MIN_SECTION_ROWS);
    let rows = split_rows(body, &[third, third, body.height.saturating_sub(third * 2)]);
    if let Some(area) = rows.first() {
        draw_system(buffer, *area, state, presentation, SHARED_BOTTOM);
    }
    if let Some(area) = rows.get(1) {
        draw_process(buffer, *area, state, presentation, SHARED_BOTTOM);
    }
    if let Some(area) = rows.get(2) {
        draw_diagnostics(buffer, *area, state, presentation, Borders::ALL);
    }
}

// ---------------------------------------------------------------------------
// System subsection
// ---------------------------------------------------------------------------

/// Draws the system subsection of §7.5.
fn draw_system(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let probe = inner_of(presentation, area, borders);
    let mut lines = system_lines(state, presentation, probe.width);
    // §7.5 puts collector health, sample and render timing, and monitrs's own
    // overhead in the *system* subsection; only the dropped/coalesced counts and
    // the collector errors belong to the diagnostics one.
    lines.extend(collector_lines(state, presentation, probe.width));
    lines.extend(render_lines(state, presentation, probe.width));
    lines.extend(overhead_lines(state, presentation, probe.width));
    draw_section(buffer, area, presentation, "SYSTEM", &lines, borders);
}

/// Every fact the system subsection lists, in §7.5's order.
fn system_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let units = presentation.units();
    let mut lines = Vec::new();
    let Some(snapshot) = state.snapshot() else {
        lines.push(muted_line(presentation, width, "warming up"));
        return lines;
    };
    let host = &snapshot.host;

    lines.push(fact(
        presentation,
        width,
        "host",
        &text_display(&host.hostname),
    ));
    lines.push(fact(
        presentation,
        width,
        "os",
        &joined_display(&host.os_name, &host.os_version),
    ));
    lines.push(fact(
        presentation,
        width,
        "kernel",
        &text_display(&host.kernel_version),
    ));
    lines.push(plain_fact(presentation, width, "arch", host.arch));
    lines.push(plain_fact(
        presentation,
        width,
        "uptime",
        &format!(
            "{} (booted {})",
            states::describe(&host.uptime, |value| {
                monitrs_core::units::format_uptime(*value)
            })
            .flagged()
            .trim_start(),
            states::describe(&host.boot_time, |value| wall_clock_of(*value))
                .flagged()
                .trim_start()
        ),
    ));
    lines.push(plain_fact(presentation, width, "cpu", &cpu_text(snapshot)));
    lines.push(plain_fact(
        presentation,
        width,
        "cpu model",
        &format!(
            "{} at {}",
            text_display(&host.cpu_brand).flagged().trim_start(),
            states::describe(&snapshot.cpu.frequency_mhz, |mhz| format!("{mhz} MHz"))
                .flagged()
                .trim_start()
        ),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "memory",
        &format!(
            "{} total, swap {}",
            format_bytes(snapshot.memory.total_bytes, units),
            swap_total_text(snapshot, units)
        ),
    ));
    // §8.4, §26: the two platforms do not share memory semantics, so the snapshot
    // says which definition produced its headline numbers and the screen shows it.
    lines.push(wrapped_fact(
        presentation,
        width,
        "semantics",
        snapshot.memory.semantics.description(),
    ));
    // §9.2: a cgroup limit is shown beside the host total, never folded into it — and
    // the group's own charge beside the limit, because `memory` above is the *host's*
    // (`/proc/meminfo` is not namespaced) and the two must not be read as a pair.
    match cgroup_memory_text(snapshot, units) {
        // A limit with the group's own charge beside it: composed text, so plain.
        Some(text) => lines.push(plain_fact(presentation, width, "cgroup limit", &text)),
        // Just the limit, in its own right — including its unavailability, which
        // `fact` styles and marks. Composing this one by hand is how `n/a` became
        // `-n/a`: `flagged` prefixes the marker that the styled path draws itself.
        None => lines.push(fact(
            presentation,
            width,
            "cgroup limit",
            &states::describe_bytes(&snapshot.memory.cgroup_limit_bytes, units),
        )),
    }
    // §7.5 requires the container/VM hint to be clearly labelled heuristic, which
    // means its evidence and its confidence travel with it.
    lines.push(fact(
        presentation,
        width,
        "environment",
        &states::describe(&host.environment, environment_text),
    ));
    lines
}

/// `8 logical, 8 physical` — the CPU counts §7.5 asks for, plus the cgroup ceiling.
///
/// The quota goes on this row rather than one of its own because it qualifies these two
/// numbers: inside a container the host's eight CPUs are still eight CPUs and still the
/// wrong figure to reason about, and a reader who sees them without the ceiling beside
/// them has been told a true thing that will mislead them. Keeping it here also costs no
/// row — as three separate rows, this information pushed `OWN OVERHEAD` off a 140x38
/// screen, and losing monitrs's own cost in order to describe the container is a poor
/// trade.
fn cpu_text(snapshot: &SystemSnapshot) -> String {
    let physical = states::describe_display(&snapshot.cpu.physical_count);
    let counts = format!(
        "{} logical, {} physical",
        snapshot.cpu.logical_count,
        physical.flagged().trim_start()
    );
    match cgroup_cpu_text(snapshot) {
        Some(quota) => format!("{counts}, {quota}"),
        None => counts,
    }
}

/// `cgroup 1.5 CPUs` — the ceiling a quota imposes, if it does.
///
/// `None` where there is no cgroup to report: `unsupported` would repeat what the
/// `cgroup limit` row below already says, and on the first frame `warming up` would
/// appear and then vanish, which reads as a glitch rather than as information. A
/// *denied* read keeps its text — a quota that exists and could not be read is exactly
/// what this screen is for (§4).
///
/// The raw `quota/period` pair is deliberately *not* here, though
/// [`CpuQuota`](monitrs_core::model::CpuQuota) carries it. It was, and the value column
/// truncated it — `8 logical, 8 physical, cgroup 1.5 CPUs (150000/1000...` — which loses
/// the period, the only thing the pair adds over the ratio, while making the row look
/// broken. The ceiling is the figure a user acts on; the pair is for whoever is reading
/// `cpu.max` itself, and they have the file.
fn cgroup_cpu_text(snapshot: &SystemSnapshot) -> Option<String> {
    if matches!(
        snapshot.cpu.cgroup_quota,
        MetricState::Unsupported | MetricState::WarmingUp
    ) {
        return None;
    }
    let display = states::describe(&snapshot.cpu.cgroup_quota, |quota| {
        format!("{:.1} CPUs", quota.cores())
    });
    Some(format!("cgroup {}", display.flagged().trim_start()))
}

/// The configured swap size, or `off` when swap is disabled.
fn swap_total_text(snapshot: &SystemSnapshot, units: ByteUnits) -> String {
    let swap = &snapshot.memory.swap;
    if swap.is_enabled() {
        format_bytes(swap.total_bytes, units)
    } else {
        "off".to_owned()
    }
}

/// `2.0G, 512M used (25%)` — the cgroup memory limit and the charge against it.
///
/// Both halves come from the group or neither does. Pairing the host's `used` with a
/// container's limit is the specific arithmetic §9.2 exists to prevent: it reports 40 GiB
/// of 2 GiB, and the resulting 2000% looks like a bug in the monitor rather than a
/// category error in the comparison.
fn cgroup_memory_text(snapshot: &SystemSnapshot, units: ByteUnits) -> Option<String> {
    let memory = &snapshot.memory;
    // `None` hands the row back to the styled path: with no charge to show there is
    // nothing to compose, and the limit's own state says everything there is to say.
    let &used = memory.cgroup_used_bytes.fresh()?;
    let charge = format_bytes_compact(used, units);
    let Some(&ceiling) = memory.cgroup_limit_bytes.fresh() else {
        // A charge with no limit above it: a cgroup that accounts but does not
        // restrict, which is the default for most units on a systemd host. The figure
        // is real and worth showing; there is simply nothing to be a percentage of.
        return Some(format!(
            "{}, {charge} used",
            states::describe_bytes(&memory.cgroup_limit_bytes, units)
                .flagged()
                .trim_start()
        ));
    };
    let limit = format_bytes_compact(ceiling, units);
    Some(match Percent::ratio(used, ceiling) {
        Some(share) => format!("{limit}, {charge} used ({share})"),
        None => format!("{limit}, {charge} used"),
    })
}

/// The heuristic environment classification with its evidence and confidence.
fn environment_text(environment: &HostEnvironment) -> String {
    // The identity, where the evidence named one, goes immediately after the
    // classification rather than on a row of its own — and ahead of the evidence, which
    // is the part a narrow panel truncates. "Which container" survives; "how we guessed"
    // is what gets cut, and that is the right order to lose them in.
    let kind = match &environment.container {
        Some(container) => format!("{} {}", environment.kind.label(), container.label()),
        None => environment.kind.label().to_owned(),
    };
    format!(
        "{kind} (heuristic, {} confidence: {})",
        environment.confidence.label(),
        environment.evidence
    )
}

// ---------------------------------------------------------------------------
// Selected-process subsection
// ---------------------------------------------------------------------------

/// Draws the selected-process subsection of §7.5.
fn draw_process(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let probe = inner_of(presentation, area, borders);
    let lines = process_lines(state, presentation, probe.width);
    draw_section(
        buffer,
        area,
        presentation,
        "SELECTED PROCESS",
        &lines,
        borders,
    );
}

/// Every fact the selected-process subsection lists, in §7.5's order.
///
/// The expensive fields come from [`AppState::detail`], which is collected on
/// demand for the selected process only (§8.6, §2.4). A detail record for a
/// *different* process is ignored rather than shown against this row: a late reply
/// for a process the user has moved off would be the wrong data under the right
/// heading (§26).
fn process_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let units = presentation.units();
    let mut lines = Vec::new();
    let Some(process) = state.selected_process() else {
        lines.push(muted_line(presentation, width, "no process selected"));
        return lines;
    };
    let detail = state
        .detail()
        .filter(|detail| detail.identity == process.identity);

    lines.push(plain_fact(
        presentation,
        width,
        "identity",
        &identity_text(process.identity),
    ));
    lines.push(plain_fact(presentation, width, "name", &process.name));
    lines.push(plain_fact(
        presentation,
        width,
        "executable",
        process.exe.as_deref().unwrap_or("-"),
    ));
    lines.push(wrapped_fact(
        presentation,
        width,
        "command",
        process.command_or_name(),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "state",
        process.state.label(),
    ));
    lines.push(fact(
        presentation,
        width,
        "user",
        &states::describe(
            &process.user,
            monitrs_core::model::UserIdentity::display_name,
        ),
    ));
    lines.push(fact(
        presentation,
        width,
        "cpu",
        &states::describe_percent(&process.cpu),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "memory",
        &process_memory_text(process, presentation),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "disk",
        &process_io_text(process, units),
    ));
    lines.push(fact(
        presentation,
        width,
        "threads",
        &states::describe_display(&process.threads),
    ));
    lines.push(fact(
        presentation,
        width,
        "age",
        &states::describe_age(&process.age),
    ));

    let Some(detail) = detail else {
        // Not an error: §8.6 collects these on demand, so "not requested yet" is the
        // normal state until the user inspects the row.
        lines.push(muted_line(
            presentation,
            width,
            "  on-demand detail not loaded",
        ));
        return lines;
    };
    lines.extend(detail_lines(detail, presentation, width, process));
    lines
}

/// The on-demand half of the process subsection (§2.4, §8.6).
fn detail_lines(
    detail: &ProcessDetail,
    presentation: Presentation<'_>,
    width: u16,
    process: &ProcessSnapshot,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(wrapped_fact_display(
        presentation,
        width,
        "cwd",
        &text_display(&detail.working_directory),
    ));
    lines.push(fact(
        presentation,
        width,
        "root",
        &text_display(&detail.root),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "parent",
        &process
            .parent_pid
            .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
    ));
    // §2.4's ancestry breadcrumb, nearest parent first.
    lines.push(wrapped_fact_display(
        presentation,
        width,
        "ancestry",
        &states::describe(&detail.ancestry, |entries: &Vec<AncestorEntry>| {
            if entries.is_empty() {
                "-".to_owned()
            } else {
                entries
                    .iter()
                    .map(|entry| format!("{}({})", entry.name, entry.identity.pid))
                    .collect::<Vec<_>>()
                    .join(" < ")
            }
        }),
    ));
    lines.push(fact(
        presentation,
        width,
        "children",
        &states::describe(&detail.children, |children: &Vec<ProcessIdentity>| {
            children.len().to_string()
        }),
    ));
    lines.push(fact(
        presentation,
        width,
        "descendants",
        &states::describe_display(&detail.descendants),
    ));
    lines.push(fact(
        presentation,
        width,
        "open files",
        &states::describe_display(&detail.open_files),
    ));
    lines.push(fact(
        presentation,
        width,
        "sockets",
        &states::describe_display(&detail.sockets),
    ));
    lines.push(fact(
        presentation,
        width,
        "nice",
        &states::describe_display(&detail.nice),
    ));
    lines.push(fact(
        presentation,
        width,
        "cgroup",
        &text_display(&detail.cgroup),
    ));
    lines.push(fact(
        presentation,
        width,
        "container",
        &text_display(&detail.container),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "collected",
        &wall_clock_of(detail.collected_at),
    ));
    lines
}

/// `2.6G rss, 15G virt, 8.1% of total` — the process memory figures of §7.5.
fn process_memory_text(process: &ProcessSnapshot, presentation: Presentation<'_>) -> String {
    let units = presentation.units();
    let rss = states::describe_bytes(&process.memory.rss_bytes, units);
    let virt = states::describe_bytes(&process.memory.virtual_bytes, units);
    let share = states::describe_percent(&process.memory.share_of_total);
    format!(
        "{} rss, {} virt, {} of total",
        rss.flagged().trim_start(),
        virt.flagged().trim_start(),
        share.flagged().trim_start()
    )
}

/// `18M/s read, 42M/s write (12G / 24G total)` — the process I/O figures.
fn process_io_text(process: &ProcessSnapshot, units: ByteUnits) -> String {
    let read = states::describe_byte_rate(&process.io.read, units);
    let write = states::describe_byte_rate(&process.io.write, units);
    let read_total = states::describe_bytes(&process.io.read_total_bytes, units);
    let write_total = states::describe_bytes(&process.io.write_total_bytes, units);
    format!(
        "{} read, {} write ({} / {} total)",
        read.flagged().trim_start(),
        write.flagged().trim_start(),
        read_total.flagged().trim_start(),
        write_total.flagged().trim_start()
    )
}

/// `31842 (start key 900100)` — the stable identity §7.5 asks for.
///
/// The start key is shown because §26's rule that a PID is not an identity is
/// exactly what this screen exists to make visible.
fn identity_text(identity: ProcessIdentity) -> String {
    format!("{} (start key {})", identity.pid, identity.start_key)
}

// ---------------------------------------------------------------------------
// Diagnostics subsection
// ---------------------------------------------------------------------------

/// Draws the diagnostics subsection of §7.5.
fn draw_diagnostics(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let probe = inner_of(presentation, area, borders);
    let lines = diagnostics_lines(state, presentation, probe.width);
    draw_section(buffer, area, presentation, "DIAGNOSTICS", &lines, borders);
}

/// Every line of the diagnostics subsection, in §7.5's order.
fn diagnostics_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.extend(pressure_lines(state, presentation, width));
    lines.extend(unavailable_lines(state, presentation, width));
    lines.extend(stale_lines(state, presentation, width));
    // Errors before counters: a failing read is more urgent than how many samples
    // were coalesced, and this panel truncates from the bottom (§7.5).
    lines.extend(issue_lines(state, presentation, width));
    lines.extend(notice_lines(state, presentation, width));
    lines.extend(sampling_lines(state, presentation, width));
    lines
}

/// The active pressure rules with the measurements that produced them (§2.3).
///
/// "Active" means a signal that is not a freshly measured `normal`: a `watch` or
/// `critical` state, and any state that could not be derived at all. §2.3 requires
/// an explicit unavailable state, and a signal nothing could be measured for is
/// exactly what a reader needs to know about — it is why the radar shows `?`.
/// `Unsupported` signals are excluded, because a platform that has no PSI files is
/// not reporting a problem.
fn pressure_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading(presentation, width, "PRESSURE")];
    let Some(snapshot) = state.snapshot() else {
        lines.push(muted_line(presentation, width, "  warming up"));
        return lines;
    };
    let mut any = false;
    for signal in &snapshot.pressure.signals {
        let interesting = match signal.state.displayable() {
            Some((state, _)) => *state != PressureState::Normal,
            None => !signal.state.is_unsupported(),
        };
        if !interesting {
            continue;
        }
        any = true;
        let row = RadarRow::new(presentation, signal).with_state_width(DIAGNOSTIC_STATE_WIDTH);
        lines.push(row.styled_line(width));
        // §2.3 requires the rule that *derived* the state to be visible. A signal
        // with no derived state was not produced by a rule — the row's `?` and its
        // reason are the whole story — so the rule line is spent on the signals it
        // actually explains.
        if signal.state.displayable().is_some() {
            lines.push(row.rule_row(width).finish());
        }
    }
    if !any {
        lines.push(muted_line(
            presentation,
            width,
            "  every derived signal is normal",
        ));
    }
    lines
}

/// The unavailable metrics and why, from [`CapabilitySnapshot::unavailable`].
///
/// §7.5 asks for both halves — what is missing and the reason — and §4 requires a
/// privilege hint where elevated privileges would plausibly help.
///
/// [`CapabilitySnapshot::unavailable`]: monitrs_core::model::CapabilitySnapshot::unavailable
fn unavailable_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading(presentation, width, "UNAVAILABLE METRICS")];
    let Some(snapshot) = state.snapshot() else {
        lines.push(muted_line(presentation, width, "  warming up"));
        return lines;
    };
    let missing = snapshot.capabilities.unavailable();
    if missing.is_empty() {
        lines.push(muted_line(
            presentation,
            width,
            "  every capability is available",
        ));
        return lines;
    }
    for (name, capability) in &missing {
        lines.push(capability_line(presentation, width, name, *capability));
    }
    if snapshot.capabilities.any_permission_denied() {
        // One hint for the whole panel rather than one per metric, and it never
        // offers to escalate: §15.1 forbids monitrs escalating on its own.
        lines.push(muted_line(
            presentation,
            width,
            "  some reads were refused; running with more privilege would provide them",
        ));
    }
    lines
}

/// One capability row: its symbol, its name, and its state.
fn capability_line(
    presentation: Presentation<'_>,
    width: u16,
    name: &str,
    capability: CapabilityState,
) -> Line<'static> {
    let token = match capability {
        CapabilityState::Available => Token::Good,
        CapabilityState::PermissionDenied => Token::Watch,
        CapabilityState::Unsupported | CapabilityState::Unknown => Token::Muted,
    };
    let mut row = row_builder(presentation, width);
    row.pad(INDENT);
    // §5.2: the symbol carries the state, so the row is readable with colour off.
    row.push(&capability.symbol().to_string(), presentation.style(token));
    row.push(" ", presentation.style(Token::Muted));
    row.push_field(
        name,
        NAME_WIDTH,
        Align::Left,
        presentation.style(Token::Text),
    );
    row.push(capability.label(), presentation.style(token));
    row.finish()
}

/// The stale-data warnings §7.5 asks for: which headline metric, and how old.
fn stale_lines(state: &AppState, presentation: Presentation<'_>, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![heading(presentation, width, "STALE DATA")];
    let Some(snapshot) = state.snapshot() else {
        lines.push(muted_line(presentation, width, "  warming up"));
        return lines;
    };
    let mut any = false;
    for (name, age) in stale_metrics(snapshot) {
        any = true;
        let mut row = row_builder(presentation, width);
        row.pad(INDENT);
        row.push("~ ", presentation.style(Token::Stale));
        row.push_field(
            name,
            NAME_WIDTH,
            Align::Left,
            presentation.style(Token::Text),
        );
        row.push(
            &format!("measured {} ago", format_age(age)),
            presentation.style(Token::Stale),
        );
        lines.push(row.finish());
    }
    // The Time Lens is the other way the screen can be showing something that is
    // not now, and §26 requires it to be unmistakable wherever it appears.
    let status = state.timeline_status();
    if status.is_frozen() {
        any = true;
        lines.push(plain_fact(
            presentation,
            width,
            "timeline",
            &format!(
                "{} — the whole frame is {} behind live",
                status.label(),
                format_age(status.behind())
            ),
        ));
    }
    if !any {
        lines.push(muted_line(
            presentation,
            width,
            "  every headline metric is fresh",
        ));
    }
    lines
}

/// The headline metrics currently showing a retained value, with their ages.
///
/// A fixed list rather than a scan of every field: these are the numbers the
/// header and the Overview put on screen, so these are the ones whose staleness
/// changes what the reader believes (§4).
fn stale_metrics(snapshot: &SystemSnapshot) -> Vec<(&'static str, Duration)> {
    let mut out = Vec::new();
    let mut push = |name: &'static str, age: Option<Duration>| {
        if let Some(age) = age {
            out.push((name, age));
        }
    };
    push("cpu total", stale_age(&snapshot.cpu.total));
    push("cpu per core", stale_age(&snapshot.cpu.per_core));
    push("memory usage", stale_age(&snapshot.memory.usage));
    push("memory used", stale_age(&snapshot.memory.used));
    push("swap used", stale_age(&snapshot.memory.swap.used));
    push("load average", stale_age(&snapshot.load));
    for disk in &snapshot.disks {
        push("disk read", stale_age(&disk.read));
        push("disk write", stale_age(&disk.write));
    }
    for interface in &snapshot.networks {
        push("network rx", stale_age(&interface.rx));
        push("network tx", stale_age(&interface.tx));
    }
    out
}

/// The age of a retained value, or `None` when the metric is not stale.
fn stale_age<T>(state: &MetricState<T>) -> Option<Duration> {
    match state {
        MetricState::Stale { age, .. } => Some(*age),
        _ => None,
    }
}

/// The dropped/coalesced counts §7.5 asks the diagnostics subsection for (§10.3).
fn sampling_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let health = state.health();
    let mut lines = vec![heading(presentation, width, "SAMPLING")];
    lines.push(plain_fact(
        presentation,
        width,
        "interval",
        &format_duration(state.sample_interval()),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "dropped",
        &health.dropped_samples.to_string(),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "coalesced",
        &health.coalesced_samples.to_string(),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "lag",
        &format_duration(health.lag),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "history",
        &format!(
            "{} samples of {}, {}",
            state.history().len(),
            state.history().capacity(),
            format_bytes_compact(
                u64::try_from(state.history().estimated_bytes()).unwrap_or(u64::MAX),
                presentation.units()
            )
        ),
    ));
    if state.rows().cycles_broken() > 0 {
        // A parent link the kernel reported that could not be believed. Rare, and
        // worth saying, because it changes what tree mode is showing.
        lines.push(plain_fact(
            presentation,
            width,
            "tree cycles",
            &state.rows().cycles_broken().to_string(),
        ));
    }
    lines
}

/// The per-tier collector timing §7.5 puts in the system subsection (§8.6, §16.1).
fn collector_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let health = state.health();
    let mut lines = vec![heading(presentation, width, "COLLECTOR")];
    for tier in Tier::ALL {
        lines.push(tier_line(presentation, width, health, tier));
    }
    lines
}

/// One collector tier's timing row.
fn tier_line(
    presentation: Presentation<'_>,
    width: u16,
    health: &CollectorHealth,
    tier: Tier,
) -> Line<'static> {
    let timing = health.tier(tier);
    let text = if timing.has_sampled() {
        format!(
            "last {} p95 {} max {} ok {} failed {}",
            format_duration(timing.last_duration),
            format_duration(timing.p95_duration),
            format_duration(timing.max_duration),
            timing.completed,
            timing.failed
        )
    } else {
        // §8.2: a tier that has not completed a collection has no timing, and zero
        // would claim it ran instantly.
        "not sampled yet".to_owned()
    };
    plain_fact(presentation, width, tier.label(), &text)
}

/// The render timing §7.5 asks for, against §16.1's budget.
fn render_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let timing: &RenderTiming = state.render_timing();
    let mut lines = vec![heading(presentation, width, "RENDER")];
    if timing.frames() == 0 {
        lines.push(muted_line(presentation, width, "  no frame drawn yet"));
        return lines;
    }
    lines.push(plain_fact(
        presentation,
        width,
        "frames",
        &format!("{}, {} over budget", timing.frames(), timing.slow_frames()),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "duration",
        &format!(
            "last {} max {} p95 {} of {} budget",
            format_duration(timing.last()),
            format_duration(timing.max()),
            timing
                .p95()
                .map_or_else(|| "n/a".to_owned(), format_duration),
            format_duration(FRAME_BUDGET)
        ),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "frame gap",
        &timing
            .last_interval()
            .map_or_else(|| "n/a".to_owned(), format_duration),
    ));
    lines
}

/// monitrs's own overhead (§16.1, §26: a monitor must expose what it costs).
fn overhead_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = vec![heading(presentation, width, "OWN OVERHEAD")];
    let Some(overhead) = state.health().self_overhead else {
        lines.push(muted_line(presentation, width, "  not measured yet"));
        return lines;
    };
    let units = presentation.units();
    lines.push(plain_fact(
        presentation,
        width,
        "cpu",
        &format!(
            "{} of one core, {} resident",
            overhead.cpu,
            format_bytes(overhead.rss_bytes, units)
        ),
    ));
    lines.push(plain_fact(
        presentation,
        width,
        "history",
        &format!(
            "{}, {} open files",
            format_bytes(overhead.history_bytes, units),
            states::describe_display(&overhead.open_files)
                .flagged()
                .trim_start()
        ),
    ));
    lines
}

/// The collector errors §7.5 asks for, aggregated rather than one row per event.
fn issue_lines(state: &AppState, presentation: Presentation<'_>, width: u16) -> Vec<Line<'static>> {
    let issues = &state.health().issues;
    let mut lines = vec![heading(presentation, width, "COLLECTOR ERRORS")];
    if issues.is_empty() {
        lines.push(muted_line(presentation, width, "  none recorded"));
        return lines;
    }
    for issue in issues {
        let age = issue
            .last_seen
            .map_or_else(String::new, |age| format!(", {} ago", format_age(age)));
        let mut row = row_builder(presentation, width);
        row.pad(INDENT);
        row.push("! ", presentation.style(Token::Watch));
        row.push(
            &format!(
                "{}: {} (x{}{age})",
                issue.source, issue.message, issue.occurrences
            ),
            presentation.style(Token::Text),
        );
        lines.push(row.finish());
    }
    lines
}

/// The notice log, which the status line can only summarise (§14.1).
fn notice_lines(
    state: &AppState,
    presentation: Presentation<'_>,
    width: u16,
) -> Vec<Line<'static>> {
    let log = state.notice_log();
    let mut lines = vec![heading(presentation, width, "NOTICES")];
    if log.is_empty() {
        lines.push(muted_line(presentation, width, "  none"));
        return lines;
    }
    for notice in log.as_slice().iter().rev() {
        let mut row = row_builder(presentation, width);
        row.pad(INDENT);
        row.push(&notice.render(), presentation.style(notice.token()));
        lines.push(row.finish());
    }
    if log.dropped() > 0 {
        lines.push(muted_line(
            presentation,
            width,
            &format!("  {} older notices were evicted", log.dropped()),
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Line builders
// ---------------------------------------------------------------------------

/// Draws one section: a panel, its lines, and a trailing truncation label.
///
/// The screen holds no scroll offset — §7.5's content is longer than any terminal
/// and [`AppState`] has nowhere to keep one — so a section that overflows says how
/// much it is showing rather than dropping the remainder silently.
fn draw_section(
    buffer: &mut Buffer,
    area: Rect,
    presentation: Presentation<'_>,
    title: &str,
    lines: &[Line<'static>],
    borders: Borders,
) {
    let probe = inner_of(presentation, area, borders);
    let visible = usize::from(probe.height).min(lines.len());
    let trailing = truncation_label(visible, lines.len()).map(|label| format!("{label} lines"));
    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        title,
        trailing.as_deref(),
        false,
        borders,
    );
    write_lines(buffer, inner, lines);
}

/// A sub-block heading inside the diagnostics panel.
///
/// Unindented, so the indented entries below it read as its contents without the
/// heading needing a colour of its own (§5.2).
fn heading(presentation: Presentation<'_>, width: u16, text: &str) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    row.push(text, presentation.style(Token::Accent));
    row.finish()
}

/// A `label   value` row whose value carries its own availability (§4).
fn fact(
    presentation: Presentation<'_>,
    width: u16,
    label: &str,
    display: &MetricDisplay,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    row.pad(INDENT);
    row.push_field(
        label,
        LABEL_WIDTH,
        Align::Left,
        presentation.style(Token::Muted),
    );
    let remaining = row.remaining();
    row.push_field(
        &display.fitted(usize::from(remaining), presentation.glyphs()),
        remaining,
        Align::Left,
        presentation.metric_style(display),
    );
    row.finish()
}

/// A `label   value` row for a value that is a plain fact, not a metric.
///
/// Used where the model has no [`MetricState`] because there is nothing the OS
/// could withhold: the architecture, a process state, a configured interval.
fn plain_fact(
    presentation: Presentation<'_>,
    width: u16,
    label: &str,
    value: &str,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    row.pad(INDENT);
    row.push_field(
        label,
        LABEL_WIDTH,
        Align::Left,
        presentation.style(Token::Muted),
    );
    let remaining = row.remaining();
    row.push_field(
        &states::fit_within(value, usize::from(remaining), presentation.glyphs()),
        remaining,
        Align::Left,
        presentation.style(Token::Text),
    );
    row.finish()
}

/// A `label   value` row whose value is middle-truncated.
///
/// For command lines and paths, whose two ends both carry information (§5.4).
fn wrapped_fact(
    presentation: Presentation<'_>,
    width: u16,
    label: &str,
    value: &str,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    row.pad(INDENT);
    row.push_field(
        label,
        LABEL_WIDTH,
        Align::Left,
        presentation.style(Token::Muted),
    );
    let remaining = row.remaining();
    row.push_field(
        &states::fit_middle_within(value, usize::from(remaining), presentation.glyphs()),
        remaining,
        Align::Left,
        presentation.style(Token::Text),
    );
    row.finish()
}

/// [`wrapped_fact`] for a value that may be unavailable.
fn wrapped_fact_display(
    presentation: Presentation<'_>,
    width: u16,
    label: &str,
    display: &MetricDisplay,
) -> Line<'static> {
    if display.is_placeholder() {
        return fact(presentation, width, label, display);
    }
    wrapped_fact(presentation, width, label, display.text())
}

/// A `Box<str>` metric as a display, so its availability reaches the cell.
fn text_display(state: &MetricState<Box<str>>) -> MetricDisplay {
    states::describe(state, |value| value.to_string())
}

/// Two `Box<str>` metrics joined, unavailable if either is.
///
/// `os` is `fake-os 1.0`, and if the version is missing the name still shows —
/// which is why this is a join and not a single field in the model.
fn joined_display(first: &MetricState<Box<str>>, second: &MetricState<Box<str>>) -> MetricDisplay {
    let head = states::describe(first, |value| value.to_string());
    let tail = states::describe(second, |value| value.to_string());
    if head.is_placeholder() {
        return head;
    }
    if tail.is_placeholder() {
        return head;
    }
    states::describe(
        &MetricState::Available(format!("{} {}", head.text(), tail.text())),
        Clone::clone,
    )
}

/// The rendered text of a line, for assertions.
#[cfg(test)]
fn text_of(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Instant, SystemTime};

    use monitrs_core::model::{
        Confidence, EnvironmentKind, MeasuredValue, Measurement, PressureId, PressureSignal,
        SelfOverhead, TierHealth,
    };
    use monitrs_core::units::display_width;

    use super::*;
    use crate::app::{AppSettings, AppState};
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn state_with(snapshot: SystemSnapshot) -> AppState {
        let mut state = AppState::new(AppSettings {
            size: (160, 48),
            ..AppSettings::default()
        });
        let _ = crate::app::apply(
            &mut state,
            crate::event::Event::<()>::Snapshot(Arc::new(snapshot)),
        );
        state
    }

    fn snapshot() -> SystemSnapshot {
        SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8)
    }

    fn joined(lines: &[Line<'static>]) -> String {
        lines.iter().map(text_of).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn no_environment_variable_ever_reaches_the_screen() {
        // §7.5 and §15.2. `ProcessDetail` has no field for them, so this asserts the
        // screen has not invented one.
        let mut snapshot = snapshot();
        snapshot.processes = vec![process()];
        let mut state = state_with(snapshot);
        let identity = state.selected().expect("a selection");
        let detail = ProcessDetail::pending(identity, SystemTime::UNIX_EPOCH);
        let _ = crate::app::apply(
            &mut state,
            crate::event::Event::<()>::Detail(monitrs_core::model::ProcessDetailResult::Loaded(
                Box::new(detail),
            )),
        );
        let text = joined(&process_lines(&state, presentation(), 120));
        for forbidden in ["PATH=", "env ", "environ", "ENV"] {
            assert!(
                !text.contains(forbidden),
                "the process section mentioned {forbidden}:\n{text}"
            );
        }
    }

    fn process() -> ProcessSnapshot {
        use monitrs_core::model::{ProcessIo, ProcessMemory, ProcessState, UserIdentity};
        ProcessSnapshot {
            identity: ProcessIdentity::new(31_842, 900_100),
            parent_pid: Some(1),
            name: "rustc".into(),
            command: "cargo build --release".into(),
            exe: Some("/usr/bin/rustc".into()),
            user: MetricState::Available(UserIdentity {
                uid: 501,
                name: Some("gabor".into()),
            }),
            state: ProcessState::Running,
            cpu: MetricState::Available(Percent::new(287.0).unwrap_or(Percent::ZERO)),
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Available(9),
            age: MetricState::Available(Duration::from_secs(43)),
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    #[test]
    fn the_process_section_names_the_start_key_as_well_as_the_pid() {
        // §26: a PID is not an identity, and this screen is where that is visible.
        assert_eq!(
            identity_text(ProcessIdentity::new(31_842, 900_100)),
            "31842 (start key 900100)"
        );
    }

    #[test]
    fn a_detail_record_for_another_process_is_ignored() {
        // §26: a late reply for a process the user has moved off must not be shown
        // against the wrong row.
        let mut snapshot = snapshot();
        snapshot.processes = vec![process()];
        let mut state = state_with(snapshot);
        let other = ProcessIdentity::new(999, 1);
        let detail = ProcessDetail::pending(other, SystemTime::UNIX_EPOCH);
        assert!(state.detail().is_none(), "no detail is loaded yet");
        let _ = crate::app::apply(
            &mut state,
            crate::event::Event::<()>::Detail(monitrs_core::model::ProcessDetailResult::Loaded(
                Box::new(detail),
            )),
        );
        let text = joined(&process_lines(&state, presentation(), 120));
        assert!(
            text.contains("on-demand detail not loaded"),
            "a mismatched detail must not be rendered:\n{text}"
        );
    }

    #[test]
    fn the_environment_hint_is_labelled_heuristic_with_its_evidence() {
        // §7.5 requires the container/VM hint to be clearly labelled heuristic.
        let text = environment_text(&HostEnvironment {
            kind: EnvironmentKind::Container,
            evidence: "/proc/1/cgroup names docker".into(),
            confidence: Confidence::Medium,
            container: None,
        });
        assert!(text.contains("heuristic"), "{text}");
        assert!(text.contains("medium confidence"), "{text}");
        assert!(text.contains("/proc/1/cgroup"), "{text}");
    }

    #[test]
    fn the_memory_semantics_are_named_rather_than_assumed() {
        // §8.4, §26: Linux and macOS memory are not the same measurement.
        let state = state_with(snapshot());
        let text = joined(&system_lines(&state, presentation(), 160));
        assert!(text.contains("semantics"), "{text}");
        assert!(text.contains("cross-platform baseline"), "{text}");
    }

    #[test]
    fn the_diagnostics_list_every_unavailable_capability_with_its_reason() {
        let mut snapshot = snapshot();
        snapshot.capabilities.disk_busy = CapabilityState::Unsupported;
        snapshot.capabilities.per_process_io = CapabilityState::PermissionDenied;
        let state = state_with(snapshot);
        let text = joined(&unavailable_lines(&state, presentation(), 120));
        assert!(text.contains("disk busy"), "{text}");
        assert!(text.contains("unsupported"), "{text}");
        assert!(text.contains("process I/O"), "{text}");
        assert!(text.contains("permission denied"), "{text}");
        assert!(
            text.contains("more privilege"),
            "§4 requires a privilege hint:\n{text}"
        );
        // §15.1: the hint informs, it never offers to escalate.
        assert!(!text.contains("sudo"), "{text}");
    }

    #[test]
    fn only_an_actionable_pressure_signal_reaches_the_diagnostics() {
        let mut snapshot = snapshot();
        snapshot.pressure.signals = vec![
            PressureSignal {
                id: PressureId::Cpu,
                state: MetricState::Available(PressureState::Normal),
                severity: MetricState::Available(Percent::ZERO),
                raw: None,
                rule: "cpu rule",
                held_for: None,
            },
            PressureSignal {
                id: PressureId::Memory,
                state: MetricState::Available(PressureState::Critical),
                severity: MetricState::Available(Percent::new(95.0).unwrap_or(Percent::FULL)),
                raw: Some(Measurement::new(
                    "available",
                    MeasuredValue::Bytes(1_200_000_000),
                )),
                rule: "available < 15% of total for 10 of 15 samples",
                held_for: Some(Duration::from_secs(12)),
            },
            PressureSignal::unsupported(PressureId::PsiIo, "Linux only"),
            PressureSignal::warming_up(PressureId::Swap, "awaiting samples"),
        ];
        let state = state_with(snapshot);
        let text = joined(&pressure_lines(&state, presentation(), 100));
        assert!(text.contains("MEM"), "{text}");
        assert!(
            text.contains("available < 15%"),
            "§2.3 requires the rule to be visible:\n{text}"
        );
        // `format_age` renders a sub-minute span as `mm:ss`, which is the same
        // form the AGE column uses, so the two read alike.
        assert!(text.contains("held 00:12"), "{text}");
        assert!(
            text.contains("SWAP"),
            "§2.3 requires an explicit unavailable state:\n{text}"
        );
        assert!(
            !text.contains("PSI-IO"),
            "a platform without PSI is not reporting a problem:\n{text}"
        );
    }

    #[test]
    fn a_stale_headline_metric_is_reported_with_its_age() {
        // §4: a retained value may only be shown alongside its age, and §7.5 asks
        // for the warning explicitly.
        let mut snapshot = snapshot();
        snapshot.memory.usage = MetricState::Available(Percent::new(71.0).unwrap_or(Percent::ZERO))
            .into_stale(Duration::from_secs(9));
        let state = state_with(snapshot);
        let text = joined(&stale_lines(&state, presentation(), 120));
        assert!(text.contains("memory usage"), "{text}");
        assert!(text.contains("00:09"), "{text}");
    }

    #[test]
    fn a_frozen_timeline_is_reported_as_stale_data_too() {
        // §26: historical state must be unmistakable wherever it appears, and the
        // Inspect screen is where a reader checks whether the numbers are current.
        let mut state = state_with(snapshot());
        let _ = crate::app::reduce(&mut state, crate::action::Action::TogglePause);
        let text = joined(&stale_lines(&state, presentation(), 120));
        assert!(text.contains("PAUSED"), "{text}");
        assert!(text.contains("behind live"), "{text}");
    }

    #[test]
    fn the_dropped_and_coalesced_counts_are_shown() {
        // §7.5 asks for both; §10.3 makes them the definition of a lagging UI.
        let mut state = state_with(snapshot());
        let health = CollectorHealth {
            dropped_samples: 3,
            coalesced_samples: 7,
            lag: Duration::from_millis(1_200),
            fast: TierHealth {
                completed: 42,
                last_duration: Duration::from_millis(3),
                p95_duration: Duration::from_millis(5),
                max_duration: Duration::from_millis(9),
                failed: 1,
                since_last: None,
            },
            ..CollectorHealth::default()
        };
        let _ = crate::app::apply(&mut state, crate::event::Event::<()>::health(health));
        let text = joined(&sampling_lines(&state, presentation(), 140));
        assert!(text.contains("dropped"), "{text}");
        assert!(text.contains("coalesced"), "{text}");
        assert!(text.contains("1.2s") || text.contains("1200ms"), "{text}");
        // Per-tier timing belongs to the system subsection, not this one (§7.5).
        let collector = joined(&collector_lines(&state, presentation(), 140));
        assert!(collector.contains("fast"), "{collector}");
        assert!(collector.contains("failed 1"), "{collector}");
        // A tier that never ran reports so instead of claiming an instant collection.
        assert!(collector.contains("not sampled yet"), "{collector}");
    }

    #[test]
    fn collector_errors_are_aggregated_with_their_counts() {
        let mut state = state_with(snapshot());
        let mut health = CollectorHealth::default();
        health.record_issue(
            "/proc/diskstats",
            "permission denied",
            Duration::from_secs(3),
        );
        health.record_issue(
            "/proc/diskstats",
            "permission denied",
            Duration::from_secs(4),
        );
        let _ = crate::app::apply(&mut state, crate::event::Event::<()>::health(health));
        let text = joined(&issue_lines(&state, presentation(), 140));
        assert!(text.contains("/proc/diskstats"), "{text}");
        assert!(
            text.contains("x2"),
            "one row with a count, not two rows:\n{text}"
        );
        assert_eq!(
            state.health().issues.len(),
            1,
            "§9.2 forbids one row per occurrence"
        );
    }

    #[test]
    fn the_render_timing_is_reported_against_the_budget() {
        // §16.1, §26: a monitor must expose what it costs.
        let mut state = state_with(snapshot());
        assert!(joined(&render_lines(&state, presentation(), 120)).contains("no frame drawn yet"));
        let at = state.clock() + Duration::from_millis(20);
        state.record_render(at, Duration::from_millis(4));
        let text = joined(&render_lines(&state, presentation(), 120));
        assert!(text.contains("frames"), "{text}");
        assert!(text.contains("budget"), "{text}");
        assert!(text.contains("16ms"), "{text}");
    }

    #[test]
    fn the_own_overhead_section_reports_what_the_collector_measured() {
        let mut state = state_with(snapshot());
        assert!(joined(&overhead_lines(&state, presentation(), 120)).contains("not measured yet"));
        let health = CollectorHealth {
            self_overhead: Some(SelfOverhead {
                cpu: Percent::new(0.4).unwrap_or(Percent::ZERO),
                rss_bytes: 24 * 1024 * 1024,
                history_bytes: 2 * 1024 * 1024,
                open_files: MetricState::Available(12),
            }),
            ..CollectorHealth::default()
        };
        let _ = crate::app::apply(&mut state, crate::event::Event::<()>::health(health));
        let text = joined(&overhead_lines(&state, presentation(), 140));
        assert!(text.contains("0.4%"), "{text}");
        assert!(text.contains("24 MiB"), "{text}");
    }

    #[test]
    fn the_notice_log_is_rendered_in_full_newest_first() {
        // The status line can only summarise it (§14.1), so this is where the whole
        // log is readable.
        let mut state = state_with(snapshot());
        state.push_notice(crate::app::Notice::info(
            crate::app::NoticeKind::Export,
            "wrote 12 KiB",
        ));
        state.push_notice(crate::app::Notice::watch(
            crate::app::NoticeKind::Permission,
            "cannot read /proc/1/io",
        ));
        let lines = notice_lines(&state, presentation(), 120);
        let text = joined(&lines);
        assert!(text.contains("cannot read"), "{text}");
        assert!(text.contains("wrote 12 KiB"), "{text}");
        let permission = text.find("cannot read").unwrap_or(usize::MAX);
        let export = text.find("wrote 12 KiB").unwrap_or(0);
        assert!(permission < export, "newest first:\n{text}");
    }

    #[test]
    fn a_missing_os_version_still_shows_the_os_name() {
        let display = joined_display(
            &MetricState::Available("Debian GNU/Linux".into()),
            &MetricState::Unsupported,
        );
        assert_eq!(display.text(), "Debian GNU/Linux");
        let missing = joined_display(&MetricState::PermissionDenied, &MetricState::Unsupported);
        assert!(missing.is_placeholder());
    }

    #[test]
    fn every_fact_row_fits_its_width() {
        // §5.7: nothing escapes its rectangle, and a fact row is assembled by hand.
        for width in [0u16, 1, 8, 20, 60, 140] {
            for line in [
                heading(presentation(), width, "PRESSURE"),
                plain_fact(presentation(), width, "arch", "aarch64"),
                wrapped_fact(presentation(), width, "command", "cargo build --release"),
                fact(
                    presentation(),
                    width,
                    "cpu",
                    &states::describe_percent(&MetricState::PermissionDenied),
                ),
                capability_line(
                    presentation(),
                    width,
                    "disk busy",
                    CapabilityState::Unsupported,
                ),
            ] {
                assert!(
                    display_width(&text_of(&line)) <= usize::from(width),
                    "a row of {} cells escaped a {width}-cell panel: {:?}",
                    display_width(&text_of(&line)),
                    text_of(&line)
                );
            }
        }
    }

    #[test]
    fn the_issue_list_is_bounded_the_way_the_model_bounds_it() {
        let mut health = CollectorHealth::default();
        for index in 0..40u32 {
            health.record_issue("source", &format!("failure {index}"), Duration::ZERO);
        }
        let mut state = state_with(snapshot());
        let _ = crate::app::apply(&mut state, crate::event::Event::<()>::health(health));
        assert_eq!(
            state.health().issues.len(),
            monitrs_core::model::MAX_RETAINED_ISSUES
        );
        let lines = issue_lines(&state, presentation(), 120);
        assert_eq!(lines.len(), monitrs_core::model::MAX_RETAINED_ISSUES + 1);
    }
}
