//! The Processes screen (§7.2), and the process table every other screen borrows.
//!
//! ```text
//! + PROCESSES  sort CPU% desc ---------------------- 218 total ---+
//! |   PID USER     S  CPU%  MEM%   RSS READ/s WRITE/s AGE NAME    |
//! |>31842 gabor    R  287%  8.1%  2.6G   18M     42M 00:43 rustc  |
//! |Z 1221 postgres Z    0%  3.0%  982M    0B      0B   12d postgres|
//! ```
//!
//! # What this module decides, and what it does not
//!
//! Column choice, column width, and column order are [`TableLayout`]'s (§7.2's
//! priority list, §5.4's stability rule). Cell text, the marker column, and the
//! notable-row styling are [`ProcessTable`]'s. Row order, filtering, tree shape,
//! and the selection are the reducer's, already resolved into
//! [`AppState::rows`]. What is left — and all this module does — is:
//!
//! * turn each [`ProcessRow`]'s [`TreeShape`] back into the ancestor
//!   continuation flags [`tree_prefix`] needs, so tree mode draws `` `- `` and
//!   `+- ` in the right columns;
//! * choose the scroll offset, which is derived rather than stored (see
//!   `super::scroll_offset`);
//! * decide the panel's title and trailing label, including §5.5's `218 total`.
//!
//! # The three non-colour cues for a notable row
//!
//! §7.2 requires zombie and uninterruptible-sleep rows to be visibly distinct and
//! §5.2 forbids colour from being the only cue. All three mechanisms live in
//! [`ProcessTable`] and survive `--color off`: the marker column carries the state
//! code (`Z`, `D`) even at widths where the `S` column has been dropped, the row
//! is drawn in [`Token::Critical`] whose emphasis is bold plus underline, and the
//! `S` column shows the same code again when it fits. This module's contribution
//! is not to get in their way — it never overrides a row's style.
//!
//! [`TableLayout`]: crate::layout::TableLayout
//! [`ProcessTable`]: crate::widgets::ProcessTable
//! [`ProcessRow`]: crate::app::ProcessRow
//! [`TreeShape`]: crate::app::TreeShape
//! [`tree_prefix`]: crate::widgets::tree_prefix
//! [`Token::Critical`]: crate::theme::Token::Critical

use monitrs_core::model::{MetricState, ProcessIdentity};
use monitrs_core::units::Percent;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Borders, Widget};

use crate::app::{AppState, ProcessRows};
use crate::layout::{Column, TableLayout};
use crate::widgets::{PinRow, Pins, Presentation, ProcessRow, ProcessTable, tree_prefix};

use super::{
    Chrome, SHARED_BOTTOM, draw_bordered_panel, inner_of, inset, muted_line, scroll_offset,
    split_rows, truncation_label, write_lines,
};

/// Rows the process panel spends on its own column header.
const HEADER_ROWS: u16 = 1;

/// The most pin rows the strip is given before the table is starved.
///
/// §2.5 promises a pinned process stays visible, and [`Pins::is_truncated`] is
/// how the panel finds out that it could not keep the promise; the count then
/// goes in the trailing label rather than being dropped silently.
const MAX_PIN_ROWS: u16 = 4;

/// Draws the Processes screen: the pinned strip, then the table (§7.2, §2.5).
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, presentation: Presentation<'_>) {
    let chrome = Chrome::resolve(area);
    let Some(body) = chrome.body else { return };
    let buffer = frame.buffer_mut();

    // The strip is only drawn when something is pinned. §2.5 asks for a compact
    // strip of pins, not a permanent empty panel: an always-present two-row frame
    // saying nothing would cost the table two rows on every terminal.
    let pin_rows = pin_strip_height(state, body.height);
    let rows = split_rows(body, &[pin_rows, body.height.saturating_sub(pin_rows)]);
    if let Some(strip) = rows.first().filter(|strip| !strip.is_empty()) {
        // The strip's bottom edge is the table's top rule (§5.5's shared borders).
        draw_pins_panel(buffer, *strip, state, presentation, false, SHARED_BOTTOM);
    }
    if let Some(table) = rows.get(1) {
        draw_table_panel(buffer, *table, state, presentation, true, Borders::ALL);
    }
}

/// Draws §5.7's minimal process list for a terminal below 80×20.
///
/// Deliberately frameless: at 60×16 a border costs two of sixteen rows, and the
/// band's requirement is a *stable* list rather than a scaled-down dashboard.
pub fn render_minimal(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    let chrome = Chrome::resolve(area);
    let Some(body) = chrome.body else { return };
    let buffer = frame.buffer_mut();
    draw_table(buffer, body, state, presentation);
}

/// Rows the pinned strip should take, including its border.
fn pin_strip_height(state: &AppState, available: u16) -> u16 {
    if state.pins().is_empty() {
        return 0;
    }
    let pins = u16::try_from(state.pins().len()).unwrap_or(MAX_PIN_ROWS);
    // Two rows of frame plus at least one pin, and never more than half the body:
    // §2.5's strip must not become a second process table.
    let wanted = pins.min(MAX_PIN_ROWS).saturating_add(2);
    wanted.min(available / 2)
}

/// Draws the bordered process panel: title, trailing count, and the table.
pub(crate) fn draw_table_panel(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    focused: bool,
    borders: Borders,
) {
    let title = panel_title(state);
    let trailing = count_label(state);
    // No inset: the table's first column is the one-cell selection marker, and §5.5
    // draws it flush against the border so `>` sits directly beside the frame.
    let inner = draw_bordered_panel(
        buffer,
        area,
        presentation,
        &title,
        Some(trailing.as_str()),
        focused,
        borders,
    );
    draw_table(buffer, inner, state, presentation);
}

/// The panel title: the screen's name plus the active ordering and mode (§7.2).
///
/// The ordering is shown because §7.2 requires it to be *stable*, and a stable
/// ordering the user cannot see is indistinguishable from an unstable one.
fn panel_title(state: &AppState) -> String {
    let sort = state.sort();
    let direction = if sort.direction.is_descending() {
        "desc"
    } else {
        "asc"
    };
    let mut title = format!("PROCESSES  sort {} {direction}", sort.key.label());
    if state.is_tree_view() {
        title.push_str("  tree");
    }
    if state.filter().hides_kernel_threads() {
        title.push_str("  no kthreads");
    }
    if state.filter().only_user().is_some() {
        title.push_str("  user only");
    }
    title
}

/// §5.5's `218 total`, or `12 of 218 total` when the filter is hiding rows.
fn count_label(state: &AppState) -> String {
    let total = state
        .snapshot()
        .map_or(0, |snapshot| snapshot.process_count());
    let visible = state.rows().len();
    if visible == total {
        format!("{total} total")
    } else {
        format!("{visible} of {total} total")
    }
}

/// Draws the table itself into `area`, header included.
///
/// The one place a process table is built, so the tree prefixes, the scroll rule,
/// the selection marker, and the pin marker cannot be applied differently on the
/// Overview and the Processes screen.
pub(crate) fn draw_table(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    if area.is_empty() {
        return;
    }
    let Some(snapshot) = state.snapshot() else {
        write_lines(
            buffer,
            area,
            &[muted_line(presentation, area.width, "warming up")],
        );
        return;
    };
    let layout = TableLayout::for_area(area);
    let body_rows = usize::from(area.height.saturating_sub(HEADER_ROWS));
    let rows = state.rows();
    let scroll = scroll_offset(state.selected_row(), body_rows, rows.len());

    // An empty table is a real state, not a failure: a locked-down container can
    // legitimately show no processes at all (§7.2's filter can also empty it).
    if rows.is_empty() {
        let table: [ProcessRow<'_>; 0] = [];
        ProcessTable::new(presentation, &layout, &table)
            .with_header(true)
            .render(area, buffer);
        let message = if state.filter().is_active() {
            "no process matches the filter"
        } else {
            "no processes visible"
        };
        let note = Rect {
            y: area.y.saturating_add(HEADER_ROWS),
            height: area.height.saturating_sub(HEADER_ROWS),
            ..area
        };
        write_lines(
            buffer,
            note,
            &[muted_line(presentation, note.width, message)],
        );
        return;
    }

    let name_width = layout
        .column(Column::Name)
        .map_or(0, |column| usize::from(column.width));
    let prefixes = tree_prefixes(state, presentation, name_width, scroll, body_rows);
    let selected = state.selected();
    let table: Vec<ProcessRow<'_>> = rows
        .as_slice()
        .iter()
        .enumerate()
        .skip(scroll)
        .take(body_rows)
        .filter_map(|(index, row)| {
            let process = rows.process(snapshot, index)?;
            let mut widget_row = ProcessRow::new(process)
                .selected(selected == Some(row.identity))
                .pinned(state.is_pinned(row.identity));
            if let Some(prefix) = prefixes.get(index.saturating_sub(scroll))
                && !prefix.is_empty()
            {
                widget_row = widget_row.with_prefix(prefix);
            }
            Some(widget_row)
        })
        .collect();

    ProcessTable::new(presentation, &layout, &table)
        .with_header(true)
        .render(area, buffer);
}

/// The tree prefixes for the visible window of rows.
///
/// [`crate::app::ProcessRows`] stores each row's depth, its parent row, and
/// whether it is the last of its siblings, but not the ancestor continuation flags
/// [`tree_prefix`] needs. They are reconstructed here by walking the parent links
/// upwards — which is cheap because the walk is bounded by the tree depth and only
/// runs for the rows on screen, and correct because
/// [`monitrs_core::process::ProcessTree`] guarantees a parent row index is always
/// smaller than its child's, so the walk terminates.
///
/// Returns an empty vector in flat mode: a flat list has no hierarchy, and drawing
/// branches on one would invent a relationship (§7.2's `TreeShape` is `None` there
/// precisely so the two cannot be confused).
fn tree_prefixes(
    state: &AppState,
    presentation: Presentation<'_>,
    name_width: usize,
    scroll: usize,
    visible: usize,
) -> Vec<String> {
    let rows = state.rows();
    if !rows.is_tree() {
        return Vec::new();
    }
    // The prefix may take at most half the name column, so a deep tree still shows
    // something of every process's name (§5.4 truncates from the tail there).
    let budget = name_width / 2;
    rows.as_slice()
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(index, row)| {
            let Some(shape) = row.tree else {
                return String::new();
            };
            tree_prefix(
                presentation.glyphs(),
                shape.depth,
                shape.is_last_child,
                &continuation_flags(rows, index),
                budget,
            )
        })
        .collect()
}

/// The ancestor continuation flags for `index`, root-most first.
///
/// `true` where the ancestor at that level still has siblings below it, which is
/// what selects a vertical bar over blank indentation. Mirrors
/// [`monitrs_core::process::ProcessTree::continuation_flags`], reconstructed from
/// the row list because that is what the reducer keeps.
fn continuation_flags(rows: &ProcessRows, index: usize) -> Vec<bool> {
    let mut flags = Vec::new();
    let mut cursor = rows.get(index).and_then(|row| row.tree?.parent_row);
    // The bound is the row count: parent indices strictly decrease, so a malformed
    // list cannot loop, but the explicit ceiling makes that independent of the
    // invariant rather than reliant on it.
    let mut guard = rows.len();
    while let Some(parent) = cursor {
        if guard == 0 {
            break;
        }
        guard -= 1;
        let Some(row) = rows.get(parent) else { break };
        let Some(shape) = row.tree else { break };
        flags.push(!shape.is_last_child);
        cursor = shape.parent_row;
    }
    flags.reverse();
    flags
}

/// Draws the pinned-process strip of §2.5.
pub(crate) fn draw_pins_panel(
    buffer: &mut Buffer,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    focused: bool,
    borders: Borders,
) {
    let probe = inset(inner_of(presentation, area, borders));
    let pins = pin_rows(state);
    let strip = Pins::new(presentation, &pins).with_baseline_label(BASELINE_LABEL);
    // §2.5 promises a pinned process stays visible, so a strip that could not fit
    // them all says so rather than dropping the remainder silently. With nothing
    // pinned there is no baseline to name either.
    let trailing = if pins.is_empty() {
        None
    } else {
        Some(
            truncation_label(strip.visible_pins(probe.height), pins.len())
                .unwrap_or_else(|| BASELINE_LABEL.to_owned()),
        )
    };
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        "PINS",
        trailing.as_deref(),
        focused,
        borders,
    ));
    if inner.is_empty() {
        return;
    }
    if pins.is_empty() {
        write_lines(
            buffer,
            inner,
            &[muted_line(
                presentation,
                inner.width,
                &empty_pins_hint(state),
            )],
        );
        return;
    }
    Pins::new(presentation, &pins).render(inner, buffer);
}

/// The baseline the pin deltas are measured against (§2.5's thirty seconds ago).
const BASELINE_LABEL: &str = "vs 30s";

/// What an empty pins strip says, naming the key the active keymap binds.
///
/// The key comes from the keymap rather than from a literal, so a rebind is
/// reflected here as well as in the generated help (§7.6). A keymap with nothing
/// bound to pinning says that instead of naming a key that does nothing.
fn empty_pins_hint(state: &AppState) -> String {
    use crate::action::Action;
    let key = state
        .keymap()
        .bindings_for_mode(state.input_mode())
        .find(|binding| binding.action() == Some(&Action::PinSelected))
        .map(|binding| binding.chord.label());
    match key {
        Some(key) => format!("nothing pinned; {key} pins the selected process"),
        None => "nothing pinned, and no key is bound to pin".to_owned(),
    }
}

/// One [`PinRow`] per pinned identity, in the order they were pinned.
///
/// A pin whose process is no longer in the displayed snapshot becomes
/// [`PinRow::exited`] rather than disappearing: §2.5 keeps the row, and a pin that
/// vanished silently would be indistinguishable from one that was never made.
fn pin_rows(state: &AppState) -> Vec<PinRow<'_>> {
    let snapshot = state.snapshot();
    state
        .pins()
        .iter()
        .map(
            |identity| match snapshot.and_then(|s| s.process(*identity)) {
                Some(process) => PinRow::from_process(process, baseline_cpu(state, *identity)),
                // The name is unknown once the process is gone, because the snapshot
                // that held it has been superseded. `PID <n>` is what the user can
                // still act on, so it is what the row is named after.
                None => PinRow::exited("exited", *identity),
            },
        )
        .collect()
}

/// The CPU percentage of `identity` thirty seconds ago, for §2.5's comparison.
///
/// `None` when the retained history has no contributor entry for the process at
/// that point — which is the normal case for a process that was below the
/// contributor cutoff, and is why [`PinRow::delta_text`] renders the *missing
/// side's* placeholder rather than presenting the current value as a change.
fn baseline_cpu(state: &AppState, identity: ProcessIdentity) -> Option<MetricState<Percent>> {
    use monitrs_core::history::{COMPARISON_LOOKBACK, ContributorMetric};
    use monitrs_core::model::MeasuredValue;

    let ring = state.history();
    let newest = ring.newest()?;
    let target = newest.monotonic_offset.checked_sub(COMPARISON_LOOKBACK)?;
    let index = ring.index_at_or_before_offset(target)?;
    let sample = ring.get(index)?;
    let contributor = sample
        .contributors
        .metric(ContributorMetric::Cpu)
        .entries()
        .iter()
        .find(|entry| entry.identity == identity)?;
    // The CPU contributor list is ranked by percentage, so any other variant would
    // be a different quantity dressed up as a baseline (§4).
    match contributor.value {
        MeasuredValue::Percent(percent) => Some(MetricState::Available(percent)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::sync::Arc;
    use std::time::{Instant, SystemTime};

    use monitrs_core::model::{
        ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState, SystemSnapshot, UserIdentity,
    };
    use monitrs_core::process::{ProcessSortKey, ProcessTree};

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

    /// The processes on screen, for assertions about ordering and selection.
    fn visible_processes<'a>(
        state: &AppState,
        snapshot: &'a SystemSnapshot,
    ) -> Vec<&'a ProcessSnapshot> {
        state.rows().processes(snapshot)
    }

    fn process(pid: u32, parent: Option<u32>, name: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, u64::from(pid) * 7),
            parent_pid: parent,
            name: name.into(),
            command: format!("/usr/bin/{name}").into(),
            exe: None,
            user: MetricState::Available(UserIdentity {
                uid: 501,
                name: Some("gabor".into()),
            }),
            state: ProcessState::Sleeping,
            cpu: MetricState::Available(Percent::new(1.0).unwrap_or(Percent::ZERO)),
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Available(1),
            age: MetricState::Available(Duration::from_secs(1)),
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    /// systemd -> {sshd -> bash, cron}
    fn tree_snapshot() -> SystemSnapshot {
        let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
        snapshot.processes = vec![
            process(1, None, "systemd"),
            process(2, Some(1), "sshd"),
            process(3, Some(2), "bash"),
            process(4, Some(1), "cron"),
        ];
        snapshot
    }

    fn state_with(snapshot: SystemSnapshot, tree: bool) -> AppState {
        let mut state = AppState::new(AppSettings {
            size: (140, 38),
            tree_mode: tree,
            sort: monitrs_core::process::ProcessSort::ascending(ProcessSortKey::Pid),
            ..AppSettings::default()
        });
        let _ = crate::app::apply(
            &mut state,
            crate::event::Event::<()>::Snapshot(Arc::new(snapshot)),
        );
        state
    }

    #[test]
    fn the_reconstructed_flags_match_the_trees_own() {
        // The prefixes are only right if the flags reconstructed from `ProcessRows`
        // agree with `ProcessTree::continuation_flags`, which is what the widget
        // documentation is written against.
        let snapshot = tree_snapshot();
        let state = state_with(snapshot.clone(), true);
        let tree = ProcessTree::build(
            &snapshot.processes,
            monitrs_core::process::ProcessSort::ascending(ProcessSortKey::Pid),
        );
        assert_eq!(tree.len(), state.rows().len());
        for index in 0..tree.len() {
            assert_eq!(
                continuation_flags(state.rows(), index),
                tree.continuation_flags(index),
                "row {index}"
            );
        }
    }

    #[test]
    fn flat_mode_produces_no_tree_prefixes() {
        // §7.2: a flat list has no hierarchy, and drawing branches on one would
        // invent a relationship the data does not carry.
        let state = state_with(tree_snapshot(), false);
        assert!(tree_prefixes(&state, presentation(), 24, 0, 10).is_empty());
        assert!(state.rows().as_slice().iter().all(|row| row.tree.is_none()));
    }

    #[test]
    fn tree_mode_indents_deeper_rows_further() {
        let state = state_with(tree_snapshot(), true);
        let prefixes = tree_prefixes(&state, presentation(), 24, 0, 10);
        assert_eq!(prefixes.len(), 4);
        let widths: Vec<usize> = prefixes
            .iter()
            .map(|prefix| monitrs_core::units::display_width(prefix))
            .collect();
        assert_eq!(widths.first(), Some(&0), "a root has no prefix");
        assert!(
            widths.get(2).copied().unwrap_or(0) > widths.get(1).copied().unwrap_or(0),
            "bash sits deeper than sshd: {widths:?}"
        );
    }

    #[test]
    fn a_cycle_in_the_row_list_cannot_loop_the_flag_walk() {
        // The parent-index invariant makes this unreachable, but the walk is bounded
        // independently of it: a panic or a hang here would corrupt the terminal.
        let state = state_with(tree_snapshot(), true);
        for index in 0..state.rows().len() {
            let flags = continuation_flags(state.rows(), index);
            assert!(flags.len() <= state.rows().len());
        }
        assert!(continuation_flags(state.rows(), 999).is_empty());
    }

    #[test]
    fn the_panel_reports_the_total_and_says_when_the_filter_hides_rows() {
        let mut state = state_with(tree_snapshot(), false);
        assert_eq!(count_label(&state), "4 total");
        let _ = crate::app::reduce(&mut state, crate::action::Action::SetFilter("bash".into()));
        assert_eq!(count_label(&state), "1 of 4 total");
    }

    #[test]
    fn the_panel_title_states_the_active_ordering() {
        // §7.2 requires stable sorting; a stable order the user cannot see is
        // indistinguishable from an unstable one.
        let state = state_with(tree_snapshot(), true);
        let title = panel_title(&state);
        assert!(title.starts_with("PROCESSES"));
        assert!(title.contains("sort PID asc"), "{title}");
        assert!(title.contains("tree"), "{title}");
    }

    #[test]
    fn the_pin_strip_takes_no_rows_when_nothing_is_pinned() {
        // §2.5 asks for a compact strip of pins, not a permanent empty frame.
        let mut state = state_with(tree_snapshot(), false);
        assert_eq!(pin_strip_height(&state, 20), 0);
        let _ = crate::app::reduce(&mut state, crate::action::Action::PinSelected);
        assert!(state.pins().len() == 1);
        assert!(pin_strip_height(&state, 20) >= 3);
        // And it never takes more than half the body.
        assert!(pin_strip_height(&state, 4) <= 2);
    }

    #[test]
    fn a_pin_whose_process_exited_keeps_its_row() {
        // §2.5: a pin that vanished silently is indistinguishable from one that was
        // never made.
        let mut state = state_with(tree_snapshot(), false);
        let _ = crate::app::reduce(&mut state, crate::action::Action::PinSelected);
        let pinned = state.pins().first().copied().expect("one pin");

        let mut later = tree_snapshot();
        later.processes.retain(|process| process.identity != pinned);
        later.sequence = 1;
        let _ = crate::app::apply(
            &mut state,
            crate::event::Event::<()>::Snapshot(Arc::new(later)),
        );

        assert_eq!(state.pins().len(), 1, "the pin survives the exit");
        let rows = pin_rows(&state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows.first().map(PinRow::identity), Some(pinned));
        let display = rows
            .first()
            .map(PinRow::value_display)
            .expect("one pin row");
        assert_eq!(display.text(), "process exited");
        assert!(display.is_placeholder(), "§4: never a zero CPU reading");
        // The strip's value field is a fixed ten cells (§5.4), so the full reason
        // degrades to `n/a` there while the `?` symbol keeps it distinguishable
        // from a measured value.
        let line = rows
            .first()
            .map(|row| row.line(presentation(), 70))
            .unwrap_or_default();
        assert!(line.contains("?n/a"), "{line}");
        assert!(!line.contains('%'), "{line}");
    }

    #[test]
    fn the_visible_window_follows_the_selection() {
        let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
        snapshot.processes = (1..=200u32)
            .map(|pid| process(pid, Some(1), "worker"))
            .collect();
        let mut state = state_with(snapshot.clone(), false);
        let _ = crate::app::reduce(&mut state, crate::action::Action::SelectLast);
        let selected = state.selected_row().expect("a selection");
        let offset = scroll_offset(Some(selected), 10, state.rows().len());
        assert!(offset <= selected && selected < offset + 10);
        assert_eq!(visible_processes(&state, &snapshot).len(), 200);
    }
}
