//! The reducer: state in, state out, effects returned (§10.2, §10.5).
//!
//! Nothing in this file performs I/O. Not a file read, not a signal, not a draw.
//! Every consequence leaves as an [`Effect`], which is what makes the §15.1
//! confirmation chain and the §6.2 keyboard model testable without a machine in a
//! particular state (§17.4).
//!
//! # Four rules that shape almost every handler
//!
//! * **No change, no redraw.** A handler that changed nothing returns no effects.
//!   §16.1 forbids a redraw busy loop, and `j` at the bottom of the table is the
//!   commonest way to cause one.
//! * **The overlay stack decides what an action means.** `Enter` picks a sort
//!   column when the selector is open, inspects a process when it is not (§6.1 has
//!   no mode for the selector, so the stack carries the context instead).
//! * **Nothing destructive happens away from live.** [`Action::is_blocked_in_history`]
//!   is checked once, at the top, for every action — including the confirmations —
//!   so no later branch has to remember (§2.1, §15.1).
//! * **A signal needs a pending action that a confirmation accepted.** The only
//!   construction of [`Effect::SignalProcess`] in this crate is
//!   [`PendingProcessAction::into_effect`], reached only from
//!   [`ProcessActionStage::Confirm`] after [`ConfirmationKind::accepts`] agreed and
//!   after the identity was revalidated against the *live* snapshot. A bare
//!   `ConfirmPendingAction` with nothing pending does nothing at all.

use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use monitrs_core::history::{
    HistoryConfig, HistoryRing, MAX_SAMPLE_INTERVAL, MIN_SAMPLE_INTERVAL, SeekOutcome,
};
use monitrs_core::model::{
    AncestorEntry, CapabilityState, ProcessDetail, ProcessDetailResult, ProcessIdentity,
    SystemSnapshot,
};
use monitrs_core::process::{ProcessSort, ProcessSortKey, SortDirection};
use monitrs_core::units::format_duration;

use crate::action::{
    Action, ConfirmationKind, Effect, Effects, PendingProcessAction, Seek, SortField, ViewId,
};
use crate::event::{Event, KeyPress, TerminalEvent};
use crate::keymap::HelpSection;

use super::command::{self, Command};
use super::notice::{Notice, NoticeKind};
use super::overlay::{Overlay, OverlayKind, ProcessActionStage};
use super::pressure::PressureAlert;
use super::text::TextInput;
use super::{AppState, IDLE_REDRAW_INTERVAL, MAX_PINNED_PROCESSES, PanelFocus, next_glyph_mode};

/// Applies one event (§10.2).
///
/// The `Cfg` payload of [`Event::ConfigReloaded`] is deliberately opaque: §10.1
/// forbids this crate from depending on the binary's configuration types, so the
/// reducer asks for a redraw and the runtime — which knows the type — reports the
/// outcome with [`AppState::push_notice`].
pub fn apply<Cfg>(state: &mut AppState, event: Event<Cfg>) -> Effects {
    match event {
        // A key press carries the mode it was resolved in, so the resolver handles
        // its own prefix bookkeeping.
        Event::Terminal(TerminalEvent::Key(press)) => key_pressed(state, press),
        other => {
            let mode_before = state.input_mode();
            let effects = apply_other(state, other);
            if state.input_mode() != mode_before {
                // §6.2: the mode changed for a reason that was not a key press — a
                // detail reply closed an overlay, say — so a half-typed `g` must not
                // complete against a different mode's table.
                state.resolver.reset();
            }
            effects
        }
    }
}

/// Applies every event that is not a key press.
fn apply_other<Cfg>(state: &mut AppState, event: Event<Cfg>) -> Effects {
    match event {
        // Already handled by `apply`; repeated here because the type does not know
        // that.
        Event::Terminal(TerminalEvent::Key(press)) => key_pressed(state, press),
        Event::Terminal(TerminalEvent::Resize { columns, rows }) => resized(state, columns, rows),
        // §6.2 defines keys only, and the guard does not enable mouse capture or
        // focus reporting by default. Inventing semantics for either would be
        // unspecified behaviour, so they are ignored rather than guessed at.
        Event::Terminal(
            TerminalEvent::Mouse(_) | TerminalEvent::FocusGained | TerminalEvent::FocusLost,
        ) => Effects::new(),
        Event::Snapshot(snapshot) => snapshot_arrived(state, snapshot),
        Event::Detail(result) => detail_arrived(state, result),
        Event::Tick(now) => ticked(state, now),
        Event::ConfigReloaded(_) => redraw(),
        Event::CollectorHealth(health) => {
            state.absorb_health(*health);
            redraw()
        }
    }
}

/// Applies one action (§10.2).
///
/// # Panics
///
/// Never. Every lookup is fallible and handled; a panic here would corrupt the
/// terminal (§14.3).
pub fn reduce(state: &mut AppState, action: Action) -> Effects {
    // §2.1, §15.1, §26: process actions are unavailable in history. Checked before
    // the match so that no handler can forget, and so that a *paused* view counts
    // as history — the PID on screen may already have exited.
    if action.is_blocked_in_history() && !state.timeline.allows_process_actions() {
        return refuse(
            state,
            Notice::watch(
                NoticeKind::Interaction,
                "process actions are disabled while the timeline is not live; press L to return \
                 to live",
            ),
        );
    }

    match action {
        // ------------------------------------------------------------ application
        Action::Quit => {
            state.should_quit = true;
            Effects::one(Effect::Shutdown)
        }
        Action::ForceRefresh => Effects::one(Effect::RequestSample),
        Action::CycleTheme => {
            state.display.theme = state.display.theme.next();
            redraw()
        }
        Action::CycleGlyphMode => {
            state.display.glyph_mode = next_glyph_mode(state.display.glyph_mode);
            redraw()
        }
        Action::ToggleHelp => {
            if state.overlays.remove(OverlayKind::Help) {
                return redraw();
            }
            state.overlays.push(Overlay::Help { scroll: 0 });
            redraw()
        }
        Action::OpenCommandPalette => {
            state.overlays.push(Overlay::CommandPalette {
                input: TextInput::new(),
                highlight: 0,
            });
            redraw()
        }
        Action::NextPanel => changed(state.cycle_focus(true)),
        Action::PreviousPanel => changed(state.cycle_focus(false)),
        Action::ChangeView(view) => change_view(state, view),

        // -------------------------------------------------------------- time lens
        Action::TogglePause => toggle_pause(state),
        Action::SeekHistory(seek) => seek_history(state, seek),
        Action::ReturnLive => return_live(state),

        // -------------------------------------------------------------- selection
        Action::SelectNext => move_cursor(state, 1),
        Action::SelectPrevious => move_cursor(state, -1),
        Action::SelectPageDown => {
            let page = i64::try_from(state.page_size()).unwrap_or(1);
            move_cursor(state, page)
        }
        Action::SelectPageUp => {
            let page = i64::try_from(state.page_size()).unwrap_or(1);
            move_cursor(state, -page)
        }
        Action::SelectFirst => jump(state, true),
        Action::SelectLast => jump(state, false),
        Action::InspectSelected => inspect_selected(state),

        // --------------------------------------------------- filtering and sorting
        Action::BeginFilterEdit => {
            let input = TextInput::seeded(&state.filter_text);
            state.overlays.push(Overlay::FilterEdit { input });
            redraw()
        }
        Action::SetFilter(text) => set_filter(state, text),
        Action::NextMatch => search(state, true),
        Action::PreviousMatch => search(state, false),
        Action::OpenSortSelector => toggle_sort_selector(state),
        Action::SetSort(field) => set_sort(state, field),
        Action::ReverseSort => {
            state.sort = state.sort.reversed();
            let _ = state.resync_rows();
            redraw()
        }
        Action::ToggleTreeView => {
            state.tree_mode = !state.tree_mode;
            let _ = state.resync_rows();
            redraw()
        }

        // ----------------------------------------------------------------- pinning
        Action::PinSelected => match state.selected() {
            Some(identity) => {
                state.selection.confirm();
                toggle_pin(state, identity)
            }
            None => Effects::new(),
        },
        Action::Pin(identity) => toggle_pin(state, identity),

        // ------------------------------------------------------------------ detail
        Action::RequestProcessDetail(identity) => request_detail(state, identity),

        // --------------------------------------------------------- process control
        Action::OpenSignalDialog => match action_target(state, Capability::Signals) {
            Some(identity) => {
                state
                    .overlays
                    .push(Overlay::ProcessAction(ProcessActionStage::ChooseSignal {
                        identity,
                        cursor: 0,
                    }));
                redraw()
            }
            None => redraw(),
        },
        Action::ProposeSignal(signal) => match action_target(state, Capability::Signals) {
            Some(identity) => propose(state, PendingProcessAction::Signal { identity, signal }),
            None => redraw(),
        },
        Action::ProposeRenice => match action_target(state, Capability::Renice) {
            Some(identity) => {
                let nice = current_nice(state, identity);
                state
                    .overlays
                    .push(Overlay::ProcessAction(ProcessActionStage::ChooseNice {
                        identity,
                        nice,
                    }));
                redraw()
            }
            None => redraw(),
        },
        // §15.1: this cannot deliver a signal. It opens the confirmation for a
        // named identity, which is what makes "no single action yields a signal
        // effect" true of the whole enum rather than of the keymap alone.
        Action::RequestSignal(identity, signal) => {
            if let Some(refusal) = revalidate(state, identity) {
                return refusal;
            }
            propose(state, PendingProcessAction::Signal { identity, signal })
        }
        Action::ConfirmPendingAction | Action::ConfirmForcefulAction => confirm(state, &action),
        Action::CancelOverlay => cancel(state),

        // ------------------------------------------------------------- text entry
        Action::InsertChar(character) => edit(state, |input| input.insert(character)),
        Action::DeleteBackward => edit(state, TextInput::delete_backward),
        Action::DeleteForward => edit(state, TextInput::delete_forward),
        Action::DeleteWordBackward => edit(state, TextInput::delete_word_backward),
        Action::ClearInput => edit(state, TextInput::clear),
        Action::MoveCursorLeft => edit(state, TextInput::move_left),
        Action::MoveCursorRight => edit(state, TextInput::move_right),
        Action::MoveCursorToStart => edit(state, TextInput::move_to_start),
        Action::MoveCursorToEnd => edit(state, TextInput::move_to_end),
        Action::SubmitInput => submit(state),
    }
}

// ---------------------------------------------------------------------- events

/// Resolves a key press in the current mode and applies whatever it produced.
///
/// Resolution uses [`AppState::clock`] rather than a fresh `Instant::now()`: the
/// reducer reads no clock of its own, so a sequence timeout is reproducible in a
/// test and a key press cannot be timed against a different clock than the tick
/// that will release it (§8.1).
fn key_pressed(state: &mut AppState, press: KeyPress) -> Effects {
    let mode = state.input_mode();
    let now = state.clock;
    let resolution = state.resolver.resolve(mode, press, now);
    let actions: Vec<Action> = resolution.actions().cloned().collect();
    let mut effects = Effects::new();
    for action in actions {
        merge(&mut effects, reduce(state, action));
    }
    effects
}

/// Records a resize (§17.5 requires this path to be tested).
///
/// Always redraws: a terminal that resized may have discarded its contents, so
/// "nothing changed in the state" is not a reason to leave the screen alone.
fn resized(state: &mut AppState, columns: u16, rows: u16) -> Effects {
    state.columns = columns;
    state.terminal_rows = rows;
    let _ = state.revalidate_focus();
    redraw()
}

/// Takes a new snapshot, coalescing rather than queueing (§10.3, §16.2).
fn snapshot_arrived(state: &mut AppState, snapshot: Arc<SystemSnapshot>) -> Effects {
    // A re-delivered or reordered snapshot carries strictly worse information than
    // the one already held. §10.3 forbids queueing old samples, so it is dropped
    // outright rather than shown for one frame.
    if state
        .latest
        .as_ref()
        .is_some_and(|latest| snapshot.sequence <= latest.sequence)
    {
        return Effects::new();
    }

    // §10.3: the previous snapshot never reached the screen, so this one supersedes
    // it. That is exactly what `CollectorHealth::coalesced_samples` counts.
    if state.unrendered_snapshot {
        state.count_coalesced();
    }

    state.advance_clock(snapshot.captured_at);
    let _ = state.history.record(&snapshot);
    state.absorb_health(snapshot.health.clone());
    let alerts = state.pressure_watch.observe(&snapshot.pressure);
    state.latest = Some(snapshot);

    // A frozen view keeps showing what it froze — unless it has never shown
    // anything. Pausing in the first second must not leave a blank screen labelled
    // PAUSED: there was nothing to freeze yet.
    if state.timeline.is_live() || state.displayed.is_none() {
        state.displayed = state.latest.clone();
        let _ = state.resync_rows();
    }
    state.unrendered_snapshot = true;

    let mut effects = redraw();
    announce_pressure(state, alerts, &mut effects);
    // §15.1: an action waiting for confirmation must not survive the process it
    // targets. Revalidating here means the dialog disappears with a reason rather
    // than confirming into nothing.
    revalidate_pending(state, &mut effects);
    effects
}

/// Records the radar transitions this snapshot produced, and rings once if asked
/// (§2.3, §11.3, §12 `diagnostics.bell_on_critical`).
///
/// Deliberately driven by the **live** snapshot, even while the timeline is paused or
/// scrubbed. §2.1 freezes what is *displayed*; collection continues, and an alert is a
/// statement about the machine now rather than about the frame on screen. Suppressing
/// alerts while paused would mean a user who paused to read one spike hears nothing
/// about the next — and holding them until the user pressed `L` would deliver a burst
/// of transitions, some of them long over, all stamped with the wrong moment. A notice
/// is safe to raise here because it opens no dialog, moves no cursor and steals no
/// key: it lands in the log and the status line, which is exactly where a user who is
/// mid-scrub can ignore it until they are ready. The destructive things §15.1 blocks
/// away from live are blocked because they *act*; this only tells.
fn announce_pressure(state: &mut AppState, alerts: Vec<PressureAlert>, effects: &mut Effects) {
    // One bell for the snapshot, however many signals escalated together: a machine
    // that runs out of memory and starts swapping crosses two thresholds in the same
    // second, and two beeps say nothing the first did not.
    let ring = state.bell_on_critical && alerts.iter().any(|alert| alert.reached_critical);
    for alert in alerts {
        state.notify(alert.notice);
    }
    if ring {
        effects.push(Effect::RingBell);
    }
}

/// Takes the answer to an on-demand detail request (§7.5, §8.6).
fn detail_arrived(state: &mut AppState, result: ProcessDetailResult) -> Effects {
    match result {
        ProcessDetailResult::Loaded(detail) => {
            let identity = detail.identity;
            // A late reply for a process the user has moved off must not be
            // rendered against the wrong row.
            if !awaited(state, identity) {
                return Effects::new();
            }
            if state.detail_request == Some(identity) {
                state.detail_request = None;
            }
            state.detail = Some(detail);
            redraw()
        }
        ProcessDetailResult::Vanished(identity) => {
            forget_process(state, identity);
            let mut effects = refuse(
                state,
                Notice::info(
                    NoticeKind::ProcessAction,
                    format!(
                        "PID {} exited before its details could be read",
                        identity.pid
                    ),
                ),
            );
            revalidate_pending(state, &mut effects);
            effects
        }
        ProcessDetailResult::Reused { requested, found } => {
            forget_process(state, requested);
            let mut effects = refuse(
                state,
                Notice::watch(
                    NoticeKind::ProcessAction,
                    format!(
                        "PID {} now belongs to a different process (start key {}); nothing was \
                         carried over",
                        requested.pid, found.start_key
                    ),
                ),
            );
            revalidate_pending(state, &mut effects);
            effects
        }
    }
}

/// Handles a timer tick (§10.2).
///
/// Calling [`crate::keymap::KeyResolver::poll_timeout`] here is not optional: §6.2
/// binds both `g` and `gg`, so a lone `g` is held as a prefix and only the timeout
/// releases its own action. Without this, `g` would appear to do nothing until the
/// next keypress.
fn ticked(state: &mut AppState, now: Instant) -> Effects {
    state.advance_clock(now);
    let mode = state.input_mode();
    let released = state.resolver.poll_timeout(mode, now);

    let mut effects = Effects::new();
    if let Some(action) = released {
        merge(&mut effects, reduce(state, action));
    }

    // The header carries a clock and relative ages, so an idle interface still has
    // to repaint — but only about once a second (§16.1: no redraw busy loop).
    let due = state
        .timing
        .last_at()
        .is_none_or(|last| now.saturating_duration_since(last) >= IDLE_REDRAW_INTERVAL);
    if due {
        effects.push(Effect::RequestRedraw);
    }
    effects
}

// --------------------------------------------------------------------- handlers

/// Switches view (§6.2 `1`–`5`).
fn change_view(state: &mut AppState, view: ViewId) -> Effects {
    if state.view == view {
        return Effects::new();
    }
    state.view = view;
    redraw()
}

/// `Space`: freezes or resumes the visible timeline (§2.1).
fn toggle_pause(state: &mut AppState) -> Effects {
    let changed_state = state.timeline.toggle_pause(&state.history);
    if !changed_state {
        return Effects::new();
    }
    if state.timeline.is_live() {
        // Resuming: catch the display up to whatever arrived while it was frozen.
        state.displayed = state.latest.clone();
        let _ = state.resync_rows();
    }
    redraw()
}

/// `[`, `]`, `{`, `}`, and the Time Lens arrow keys (§2.1).
fn seek_history(state: &mut AppState, seek: Seek) -> Effects {
    if seek.is_noop() {
        return Effects::new();
    }
    let outcome = state.timeline.seek(&state.history, seek);
    if matches!(outcome, SeekOutcome::Empty) {
        return refuse(
            state,
            Notice::info(
                NoticeKind::Interaction,
                "no samples have been recorded yet, so there is no history to scrub",
            ),
        );
    }
    // The displayed snapshot is deliberately *not* replaced: it is already the last
    // live one, and history retains aggregates and contributors rather than whole
    // process tables (§8.5). Process actions are refused while scrubbed (§15.1),
    // which is what makes showing that frozen table safe.
    redraw()
}

/// `L`: the one explicit return to live (§2.1).
fn return_live(state: &mut AppState) -> Effects {
    if !state.timeline.return_live() {
        return Effects::new();
    }
    state.displayed = state.latest.clone();
    let _ = state.resync_rows();
    redraw()
}

/// Moves whatever the topmost overlay considers its cursor, or the selection.
fn move_cursor(state: &mut AppState, delta: i64) -> Effects {
    let Some(kind) = state.overlays.top().map(Overlay::kind) else {
        return changed(state.selection.step(&state.rows, delta));
    };
    match kind {
        OverlayKind::CommandPalette => move_suggestion(state, delta),
        // The limit comes from the same content the renderer draws, so a scrolled
        // overlay stops at its last line instead of running off into an offset the
        // user then has to scroll back through.
        OverlayKind::Help | OverlayKind::ProcessDetail => {
            let lines = scroll_lines(state);
            let mut moved = false;
            if let Some(Overlay::Help { scroll } | Overlay::ProcessDetail { scroll, .. }) =
                state.overlays.top_mut()
            {
                moved = step_index(scroll, delta, lines);
            }
            changed(moved)
        }
        OverlayKind::SortSelector => {
            let mut moved = false;
            if let Some(Overlay::SortSelector { cursor }) = state.overlays.top_mut() {
                moved = step_index(cursor, delta, ProcessSortKey::ALL.len());
            }
            changed(moved)
        }
        OverlayKind::ProcessAction => {
            let mut moved = false;
            if let Some(Overlay::ProcessAction(stage)) = state.overlays.top_mut() {
                moved = stage.step(i32::try_from(delta).unwrap_or(0));
            }
            changed(moved)
        }
        OverlayKind::FilterEdit => Effects::new(),
    }
}

/// `gg`/`Home` and `G`/`End`.
fn jump(state: &mut AppState, to_first: bool) -> Effects {
    match state.overlays.top().map(Overlay::kind) {
        Some(OverlayKind::Help | OverlayKind::ProcessDetail) => {
            let limit = scroll_lines(state).saturating_sub(1);
            let mut moved = false;
            if let Some(Overlay::Help { scroll } | Overlay::ProcessDetail { scroll, .. }) =
                state.overlays.top_mut()
            {
                let target = if to_first { 0 } else { limit };
                moved = *scroll != target;
                *scroll = target;
            }
            changed(moved)
        }
        Some(OverlayKind::SortSelector) => {
            let last = ProcessSortKey::ALL.len().saturating_sub(1);
            let mut moved = false;
            if let Some(Overlay::SortSelector { cursor }) = state.overlays.top_mut() {
                let target = if to_first { 0 } else { last };
                moved = *cursor != target;
                *cursor = target;
            }
            changed(moved)
        }
        Some(_) => Effects::new(),
        None => {
            if to_first {
                changed(state.selection.first(&state.rows))
            } else {
                changed(state.selection.last(&state.rows))
            }
        }
    }
}

/// `Enter` (§6.2).
fn inspect_selected(state: &mut AppState) -> Effects {
    if let Some(Overlay::SortSelector { cursor }) = state.overlays.top() {
        let key = ProcessSortKey::ALL.get(*cursor).copied();
        state.overlays.remove(OverlayKind::SortSelector);
        let mut effects = match key {
            Some(key) => apply_sort(state, ProcessSort::new(key, state.sort.direction)),
            None => Effects::new(),
        };
        effects.push(Effect::RequestRedraw);
        return effects;
    }
    match state.selected() {
        Some(identity) => {
            state.selection.confirm();
            state.overlays.push(Overlay::ProcessDetail {
                identity,
                scroll: 0,
            });
            request_detail(state, identity)
        }
        // An empty table is a real state, not an error; `Enter` on nothing is
        // silence rather than a notice the user cannot act on.
        None => Effects::new(),
    }
}

/// Replaces the filter (§6.2 `/`, §6.3 `filter <text>`).
fn set_filter(state: &mut AppState, text: String) -> Effects {
    if !state.set_filter_text(text) {
        return Effects::new();
    }
    let _ = state.resync_rows();
    redraw()
}

/// `n` and `N`: move the selection between text matches (§6.2).
fn search(state: &mut AppState, forward: bool) -> Effects {
    let Some(snapshot) = state.displayed.clone() else {
        return Effects::new();
    };
    let from = state.selection.row();
    let found = {
        let processes = state.rows.processes(&snapshot);
        if forward {
            state.filter.next_match(&processes, from)
        } else {
            state.filter.previous_match(&processes, from)
        }
    };
    match found {
        Some(row) => changed(state.selection.select_row(&state.rows, row)),
        // No pattern, or no match: `n` does nothing rather than jumping somewhere
        // arbitrary.
        None => Effects::new(),
    }
}

/// `s`: opens or closes the sort selector (§6.2).
fn toggle_sort_selector(state: &mut AppState) -> Effects {
    if matches!(state.overlays.top(), Some(Overlay::SortSelector { .. })) {
        let _ = state.overlays.pop();
        return redraw();
    }
    let cursor = ProcessSortKey::ALL
        .iter()
        .position(|key| *key == state.sort.key)
        .unwrap_or(0);
    state.overlays.push(Overlay::SortSelector { cursor });
    redraw()
}

/// `sort <field>` and the selector's `Enter` (§6.2, §6.3).
///
/// A *different* column starts in its natural direction — consumption columns
/// descending, text columns ascending, per [`SortField::defaults_descending`] —
/// because the user asked for that column, not for the previous column's
/// direction. Re-selecting the current column changes nothing; `S` is what
/// reverses (§6.2).
fn set_sort(state: &mut AppState, field: SortField) -> Effects {
    let key = sort_key(field);
    let sort = if key == state.sort.key {
        state.sort
    } else {
        ProcessSort::new(
            key,
            SortDirection::from_descending(field.defaults_descending()),
        )
    };
    apply_sort(state, sort)
}

/// Applies an ordering, rebuilding the rows if it actually changed.
fn apply_sort(state: &mut AppState, sort: ProcessSort) -> Effects {
    if state.sort == sort {
        return Effects::new();
    }
    state.sort = sort;
    let _ = state.resync_rows();
    redraw()
}

/// `p`: pins or unpins by identity (§2.5).
fn toggle_pin(state: &mut AppState, identity: ProcessIdentity) -> Effects {
    if let Some(index) = state.pins.iter().position(|pin| *pin == identity) {
        let _ = state.pins.remove(index);
        return redraw();
    }
    if state.pins.len() >= MAX_PINNED_PROCESSES {
        return refuse(
            state,
            Notice::watch(
                NoticeKind::Interaction,
                format!("at most {MAX_PINNED_PROCESSES} processes can be pinned; unpin one first"),
            ),
        );
    }
    state.pins.push(identity);
    redraw()
}

/// Asks the detail worker for one process (§8.6).
fn request_detail(state: &mut AppState, identity: ProcessIdentity) -> Effects {
    state.detail_request = Some(identity);
    // Keep the highlighted row and the overlay talking about the same process. For
    // `Enter` this is already true; for a request that named an identity — a palette
    // command, or a pin the user jumped to — it is what stops the table and the
    // overlay from disagreeing.
    let _ = state.selection.select_identity(&state.rows, identity);
    if state
        .detail
        .as_ref()
        .is_some_and(|detail| detail.identity != identity)
    {
        // Never show one process's detail under another's heading.
        state.detail = None;
    }
    let mut effects = Effects::one(Effect::FetchProcessDetail(identity));
    effects.push(Effect::RequestRedraw);
    effects
}

/// Opens the confirmation for a resolved action (§15.1).
fn propose(state: &mut AppState, pending: PendingProcessAction) -> Effects {
    state
        .overlays
        .push(Overlay::ProcessAction(ProcessActionStage::Confirm(pending)));
    redraw()
}

/// `Enter`/`y`/`Y` inside the process-action chain (§15.1).
fn confirm(state: &mut AppState, action: &Action) -> Effects {
    // Nothing pending: a confirmation on its own is inert. This is the property
    // §17.4 asks to be proven — no single action reaches a signal effect.
    let Some(stage) = state.process_action_stage() else {
        return Effects::new();
    };

    let Some(pending) = stage.pending() else {
        // Still choosing: advance to the confirmation rather than acting.
        return propose(state, stage.resolved());
    };

    if !pending.confirmation().accepts(action) {
        let hint = ConfirmationKind::Forceful.key_hint();
        let what = match pending {
            PendingProcessAction::Signal { signal, .. } => signal.name(),
            PendingProcessAction::Renice { .. } => "renice",
        };
        return refuse(
            state,
            Notice::watch(
                NoticeKind::ProcessAction,
                format!("{what} is forceful; confirm with {hint} rather than Enter"),
            ),
        );
    }

    // §15.1: revalidate the identity before the effect leaves the reducer. The
    // executor revalidates again at delivery, because the process can exit between
    // any two OS reads (§26) — this catch is the one the *user* sees.
    if let Some(refusal) = revalidate(state, pending.identity()) {
        return refusal;
    }

    let _ = state.overlays.remove(OverlayKind::ProcessAction);
    state.notify(Notice::info(
        NoticeKind::ProcessAction,
        describe_request(pending),
    ));
    let mut effects = Effects::one(pending.into_effect());
    effects.push(Effect::RequestRedraw);
    effects
}

/// `Esc` and `n` (§6.2).
fn cancel(state: &mut AppState) -> Effects {
    if let Some(closed) = state.overlays.pop() {
        if closed.kind() == OverlayKind::ProcessDetail {
            // Stop waiting for a reply nobody will look at.
            state.detail_request = None;
        }
        return redraw();
    }
    // No overlay: leave whatever mode the focused panel implies, which is how the
    // Time Lens is left when `Tab` is not bound in that mode (§6.1).
    if state.focus == PanelFocus::Processes {
        return Effects::new();
    }
    state.focus = PanelFocus::Processes;
    redraw()
}

/// Applies a text edit to the topmost text buffer (§6.1).
fn edit(state: &mut AppState, mutate: impl FnOnce(&mut TextInput) -> bool) -> Effects {
    let Some(overlay) = state.overlays.top_mut() else {
        return Effects::new();
    };
    let Some(input) = overlay.text_input_mut() else {
        return Effects::new();
    };
    let edited = mutate(input);
    if edited {
        // The suggestion list is recomputed from the text, so the old highlight no
        // longer refers to the same command.
        if let Overlay::CommandPalette { highlight, .. } = overlay {
            *highlight = 0;
        }
    }
    changed(edited)
}

/// `Enter` in a text mode (§6.1).
fn submit(state: &mut AppState) -> Effects {
    match state.overlays.top().map(Overlay::kind) {
        Some(OverlayKind::FilterEdit) => {
            let text = state
                .overlays
                .top()
                .and_then(Overlay::text_input)
                .map(|input| input.text().to_owned())
                .unwrap_or_default();
            let _ = state.overlays.remove(OverlayKind::FilterEdit);
            let mut effects = set_filter(state, text);
            // Closing the editor is a visible change even when the filter did not
            // change.
            effects.push(Effect::RequestRedraw);
            effects
        }
        Some(OverlayKind::CommandPalette) => submit_command(state),
        _ => Effects::new(),
    }
}

/// Runs, or completes, the typed palette line (§6.3).
fn submit_command(state: &mut AppState) -> Effects {
    let (text, highlight) = match state.overlays.top() {
        Some(Overlay::CommandPalette { input, highlight }) => (input.text().to_owned(), *highlight),
        _ => return Effects::new(),
    };

    match command::parse(&text) {
        Ok(parsed) => {
            let _ = state.overlays.remove(OverlayKind::CommandPalette);
            let mut effects = run_command(state, parsed);
            effects.push(Effect::RequestRedraw);
            effects
        }
        Err(error) => {
            // A half-typed command completes towards its highlighted suggestion
            // instead of being rejected: §6.3 exists for discoverability.
            let completion = command::hints_for(&text)
                .get(highlight)
                .map(|hint| hint.completion)
                .filter(|completion| *completion != text.as_str());
            if let Some(completion) = completion {
                let mut completed = false;
                if let Some(Overlay::CommandPalette { input, .. }) = state.overlays.top_mut() {
                    completed = input.set(completion);
                }
                if completed {
                    return redraw();
                }
            }
            refuse(
                state,
                Notice::watch(NoticeKind::Interaction, error.to_string()),
            )
        }
    }
}

/// Dispatches a parsed palette command (§6.3).
fn run_command(state: &mut AppState, command: Command) -> Effects {
    match command {
        Command::ChangeView(view) => change_view(state, view),
        Command::Sort(field) => set_sort(state, field),
        Command::Filter(text) => set_filter(state, text),
        Command::Interval(interval) => set_interval(state, interval),
        Command::History(duration) => set_history_span(state, duration),
        Command::Theme(theme) => {
            state.display.theme = theme;
            redraw()
        }
        Command::Glyphs(mode) => {
            state.display.glyph_mode = mode;
            redraw()
        }
        Command::Color(mode) => {
            state.display.color_mode = mode;
            // Typing a colour mode is an explicit request, so it may override
            // `NO_COLOR` where a configured default may not (§5.2).
            state.display.color_explicit = true;
            redraw()
        }
        Command::ExportSnapshot(path) => Effects::one(Effect::ExportSnapshot(path)),
        Command::ConfigPath => {
            let message = match state.config_path() {
                Some(path) => format!("configuration is read from {}", path.display()),
                None => "no configuration file is loaded; built-in defaults are in use".to_owned(),
            };
            refuse(state, Notice::info(NoticeKind::Config, message))
        }
        Command::ReloadConfig => Effects::one(Effect::ReloadConfig),
    }
}

/// `interval <duration>` (§6.3).
///
/// Clamped to the range the history ring supports and reported when clamped, the
/// same way a configured value is (§8.5). The new interval changes the ring's
/// capacity, so the ring is rebuilt — [`set_history_span`] explains why that
/// discards what was retained.
fn set_interval(state: &mut AppState, interval: Duration) -> Effects {
    let clamped = interval.clamp(MIN_SAMPLE_INTERVAL, MAX_SAMPLE_INTERVAL);
    if clamped != interval {
        state.notify(Notice::watch(
            NoticeKind::Config,
            format!(
                "sample interval {} is outside the supported range; using {}",
                format_duration(interval),
                format_duration(clamped)
            ),
        ));
    }
    if state.sample_interval == clamped {
        return redraw();
    }
    state.sample_interval = clamped;
    state.notify(Notice::info(
        NoticeKind::Config,
        format!("sample interval set to {}", format_duration(clamped)),
    ));
    rebuild_history(
        state,
        HistoryConfig {
            interval: clamped,
            ..state.history_config
        },
    )
}

/// `history <duration>` (§6.3).
fn set_history_span(state: &mut AppState, duration: Duration) -> Effects {
    rebuild_history(
        state,
        HistoryConfig {
            duration,
            ..state.history_config
        },
    )
}

/// Rebuilds the history ring for a new configuration (§8.5).
///
/// The ring has a fixed capacity derived from span and interval, and its API takes
/// snapshots rather than retained samples, so there is no way to carry the old
/// samples across. Discarding them is therefore reported rather than hidden — and
/// the Time Lens returns to live, because absolute sample indices from the old ring
/// mean nothing in the new one.
fn rebuild_history(state: &mut AppState, config: HistoryConfig) -> Effects {
    state.history_config = config;
    state.history = HistoryRing::with_config(config, state.clock);
    state.timeline = super::Timeline::live();
    state.displayed = state.latest.clone();
    let _ = state.resync_rows();

    let limits = state.history.limits();
    let span = limits.effective_duration();
    let interval = limits.interval();
    let clamps: Vec<String> = state
        .history
        .clamps()
        .iter()
        .map(|clamp| clamp.message())
        .collect();
    for message in clamps {
        state.notify(Notice::watch(NoticeKind::Config, message));
    }
    state.notify(Notice::watch(
        NoticeKind::Config,
        format!(
            "history rebuilt for {} at {}; previously retained samples were discarded",
            format_duration(span),
            format_duration(interval)
        ),
    ));
    redraw()
}

// --------------------------------------------------------------------- helpers

/// Which capability a process action needs (§4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Capability {
    /// Sending a signal.
    Signals,
    /// Changing scheduling priority.
    Renice,
}

impl Capability {
    /// The label used in the refusal notice.
    const fn label(self) -> &'static str {
        match self {
            Self::Signals => "sending signals",
            Self::Renice => "changing scheduling priority",
        }
    }
}

/// Resolves the process a keyboard-initiated action applies to, or explains why
/// there is none.
///
/// Three refusals, all of them things §15.1 requires to be reported clearly rather
/// than attempted: nothing is selected, the platform or privilege level cannot do
/// it, or the process has already exited and a signal would be a no-op.
fn action_target(state: &mut AppState, capability: Capability) -> Option<ProcessIdentity> {
    let Some(process) = state.selected_process() else {
        state.notify(Notice::info(
            NoticeKind::Interaction,
            "no process is selected",
        ));
        return None;
    };
    let identity = process.identity;
    let name = process.name.to_string();
    let signalable = process.state.is_signalable();
    let state_label = process.state.label();
    // Aiming an action at a row is choosing it: the cursor must not drift off the
    // process the confirmation dialog is about (§7.2).
    state.selection.confirm();

    let support = state.live_snapshot().map(|snapshot| match capability {
        Capability::Signals => snapshot.capabilities.process_signals,
        Capability::Renice => snapshot.capabilities.renice,
    });
    match support {
        Some(CapabilityState::Unsupported) => {
            state.notify(Notice::watch(
                NoticeKind::ProcessAction,
                format!("{} is not supported on this platform", capability.label()),
            ));
            return None;
        }
        Some(CapabilityState::PermissionDenied) => {
            state.notify(Notice::watch(
                NoticeKind::Permission,
                format!(
                    "{} is not permitted at this privilege level; monitrs does not escalate",
                    capability.label()
                ),
            ));
            return None;
        }
        // `Unknown` means "not probed", not "unavailable" (§4). The attempt is
        // allowed and the executor reports what the OS says.
        _ => {}
    }

    if !signalable {
        state.notify(Notice::info(
            NoticeKind::ProcessAction,
            format!(
                "{name} (PID {}) is already {state_label}; the action would have no effect",
                identity.pid
            ),
        ));
        return None;
    }
    Some(identity)
}

/// The nice value a renice dialog should open on.
///
/// The loaded detail when it describes this process, otherwise `0`. Opening on a
/// guess would be worse: `0` is at least a value the user can see and adjust before
/// confirming.
fn current_nice(state: &AppState, identity: ProcessIdentity) -> i8 {
    state
        .detail()
        .filter(|detail| detail.identity == identity)
        .and_then(|detail| detail.nice.fresh().copied())
        .and_then(|nice| i8::try_from(nice).ok())
        .unwrap_or(0)
}

/// Checks that `identity` is still the process it was, refusing with a notice if
/// not (§15.1, §26).
fn revalidate(state: &mut AppState, identity: ProcessIdentity) -> Option<Effects> {
    let Some(snapshot) = state.live_snapshot() else {
        return Some(refuse(
            state,
            Notice::watch(
                NoticeKind::ProcessAction,
                "no live sample yet, so the process identity cannot be revalidated",
            ),
        ));
    };
    if snapshot.process(identity).is_some() {
        return None;
    }
    let reused = snapshot
        .process_by_pid(identity.pid)
        .map(|process| process.identity)
        .filter(|current| current.is_reuse_of(&identity));

    let notice = match reused {
        Some(current) => Notice::watch(
            NoticeKind::ProcessAction,
            format!(
                "PID {} now belongs to a different process (start key {}); the request was \
                 cancelled",
                identity.pid, current.start_key
            ),
        ),
        None => Notice::info(
            NoticeKind::ProcessAction,
            format!(
                "PID {} has already exited; the request was cancelled",
                identity.pid
            ),
        ),
    };
    let _ = state.overlays.remove(OverlayKind::ProcessAction);
    Some(refuse(state, notice))
}

/// Cancels a process action whose target has gone (§15.1).
fn revalidate_pending(state: &mut AppState, effects: &mut Effects) {
    let Some(stage) = state.process_action_stage() else {
        return;
    };
    if let Some(refusal) = revalidate(state, stage.identity()) {
        merge(effects, refusal);
    }
}

/// Forgets everything cached about a process that has gone.
fn forget_process(state: &mut AppState, identity: ProcessIdentity) {
    if state.detail_request == Some(identity) {
        state.detail_request = None;
    }
    if state
        .detail
        .as_ref()
        .is_some_and(|detail| detail.identity == identity)
    {
        state.detail = None;
    }
    if matches!(
        state.overlays.find(OverlayKind::ProcessDetail),
        Some(Overlay::ProcessDetail { identity: open, .. }) if *open == identity
    ) {
        let _ = state.overlays.remove(OverlayKind::ProcessDetail);
    }
}

/// Whether a detail reply is still wanted.
fn awaited(state: &AppState, identity: ProcessIdentity) -> bool {
    if state.detail_request == Some(identity) || state.selected() == Some(identity) {
        return true;
    }
    matches!(
        state.overlays.find(OverlayKind::ProcessDetail),
        Some(Overlay::ProcessDetail { identity: open, .. }) if *open == identity
    )
}

/// The sentence the notice shows when a request leaves the reducer (§15.1: process
/// actions and their outcomes are reported).
fn describe_request(pending: PendingProcessAction) -> String {
    match pending {
        PendingProcessAction::Signal { identity, signal } => format!(
            "requested {} for PID {}; identity is revalidated before delivery",
            signal.name(),
            identity.pid
        ),
        PendingProcessAction::Renice { identity, nice } => format!(
            "requested nice {nice} for PID {}; identity is revalidated before delivery",
            identity.pid
        ),
    }
}

/// Moves the palette's suggestion highlight (§6.3).
fn move_suggestion(state: &mut AppState, delta: i64) -> Effects {
    let count = match state.overlays.top() {
        Some(Overlay::CommandPalette { input, .. }) => command::hints_for(input.text()).len(),
        _ => 0,
    };
    if count == 0 {
        return Effects::new();
    }
    let mut moved = false;
    if let Some(Overlay::CommandPalette { highlight, .. }) = state.overlays.top_mut() {
        moved = step_index(highlight, delta, count);
    }
    changed(moved)
}

/// Moves an index by `delta`, clamped to `0..count`.
fn step_index(index: &mut usize, delta: i64, count: usize) -> bool {
    let last = count.saturating_sub(1);
    let current = i64::try_from(*index).unwrap_or(i64::MAX);
    let target = current.saturating_add(delta).max(0);
    let target = usize::try_from(target).unwrap_or(usize::MAX).min(last);
    if target == *index {
        return false;
    }
    *index = target;
    true
}

/// How many lines the topmost overlay has to show, and therefore how far it may
/// scroll.
///
/// At least one, so that an overlay with no content still has a valid offset of
/// zero rather than an empty range.
fn scroll_lines(state: &AppState) -> usize {
    let lines = match state.overlays.top().map(Overlay::kind) {
        Some(OverlayKind::Help) => help_line_count(&state.help()),
        Some(OverlayKind::ProcessDetail) => detail_line_count(state.detail()),
        _ => 0,
    };
    lines.max(1)
}

/// How many lines the generated help occupies: one heading per section plus its
/// entries (§7.6).
#[must_use]
pub fn help_line_count(sections: &[HelpSection]) -> usize {
    sections
        .iter()
        .map(|section| section.entries.len().saturating_add(1))
        .sum()
}

/// How many logical lines the process-detail overlay has to show (§7.5).
///
/// The reducer needs this to clamp scrolling, and the renderer calls *this* function
/// rather than counting for itself, so the two cannot drift apart. Fixed rows:
/// working directory, root, open files, sockets, the descriptor-listing summary,
/// descendants, nice, cgroup, container, and the collection age. Variable rows: one
/// per ancestor (§2.4's breadcrumb), one per direct child, and one per listed open
/// descriptor (§7.2) — the last of which is why the count has to be derived from the
/// record rather than being a constant.
#[must_use]
pub fn detail_line_count(detail: Option<&ProcessDetail>) -> usize {
    /// Rows that are always present, one per [`ProcessDetail`]
    /// scalar field plus the collection timestamp.
    const FIXED_ROWS: usize = 10;

    let Some(detail) = detail else {
        return 1;
    };
    let ancestry = detail
        .ancestry
        .fresh()
        .map_or(0, |entries: &Vec<AncestorEntry>| entries.len());
    let children = detail
        .children
        .fresh()
        .map_or(0, |children: &Vec<ProcessIdentity>| children.len());
    let descriptors = detail
        .open_file_list
        .fresh()
        .map_or(0, monitrs_core::model::OpenFileList::count);
    FIXED_ROWS
        .saturating_add(ancestry)
        .saturating_add(children)
        .saturating_add(descriptors)
}

/// Maps a keyboard/palette sort column onto the core comparator's key.
///
/// `Rss` and `MemoryShare` are one key because the share of total is RSS divided by
/// a constant and therefore produces the same order (§7.2 lists them at one
/// priority level). `Command` folds into `Name`, whose comparator already falls
/// back to the command line.
const fn sort_key(field: SortField) -> ProcessSortKey {
    match field {
        SortField::Pid => ProcessSortKey::Pid,
        SortField::User => ProcessSortKey::User,
        SortField::State => ProcessSortKey::State,
        SortField::Cpu => ProcessSortKey::Cpu,
        SortField::MemoryShare | SortField::Rss => ProcessSortKey::Memory,
        SortField::VirtualMemory => ProcessSortKey::Virtual,
        SortField::ReadRate => ProcessSortKey::Read,
        SortField::WriteRate => ProcessSortKey::Write,
        SortField::Threads => ProcessSortKey::Threads,
        SortField::Age => ProcessSortKey::Age,
        SortField::Name | SortField::Command => ProcessSortKey::Name,
    }
}

/// One redraw request.
fn redraw() -> Effects {
    Effects::one(Effect::RequestRedraw)
}

/// A redraw if something changed, nothing otherwise (§16.1).
fn changed(changed: bool) -> Effects {
    if changed { redraw() } else { Effects::new() }
}

/// Records a notice and asks for a redraw.
fn refuse(state: &mut AppState, notice: Notice) -> Effects {
    state.notify(notice);
    redraw()
}

/// Folds one reduction's effects into another's, keeping the dedup rules.
fn merge(into: &mut Effects, from: Effects) {
    for effect in from {
        into.push(effect);
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::model::{
        CapabilitySnapshot, CollectorHealth, MetricState, PressureId, PressureState, ProcessState,
        Severity,
    };

    use crate::action::SignalKind;
    use crate::event::Key;
    use crate::keymap::InputMode;
    use crate::theme::ThemeId;

    use super::super::fixtures::{
        Fake, all_capabilities, arc_snapshot, arc_snapshot_with_pressure, epoch, set_pressure,
        snapshot_of, snapshot_with,
    };
    use super::super::{
        AppSettings, MAX_PINNED_PROCESSES, NoticeKind, OverlayKind, PanelFocus, ProcessActionStage,
        Resync, TimelineStatus,
    };
    use super::*;

    /// The reference process table, from §5.5's mockup.
    fn table() -> Vec<Fake> {
        vec![
            Fake::new(31_842, 900_100, "rustc")
                .command("cargo build --release")
                .cpu(287.0),
            Fake::new(1_221, 700_050, "postgres")
                .command("postgres: checkpointer")
                .cpu(54.0)
                .user("postgres", 70),
            Fake::new(507, 100_010, "WindowServer").cpu(21.0),
            Fake::new(9_182, 850_300, "node").cpu(12.0),
            Fake::new(1, 1, "launchd").cpu(0.1),
        ]
    }

    fn state() -> AppState {
        AppState::new(AppSettings {
            started_at: epoch(),
            size: (160, 48),
            ..AppSettings::default()
        })
    }

    /// A state with one snapshot delivered and rendered, i.e. steady state.
    fn running() -> AppState {
        let mut state = state();
        deliver(&mut state, 1, &table());
        state
    }

    /// Delivers a snapshot and marks it rendered, as the runtime would.
    fn deliver(state: &mut AppState, sequence: u64, processes: &[Fake]) -> Effects {
        let snapshot = arc_snapshot(sequence, processes);
        let at = snapshot.captured_at;
        let effects = apply::<()>(state, Event::Snapshot(snapshot));
        state.record_render(at, Duration::from_millis(4));
        effects
    }

    fn press(state: &mut AppState, key: char) -> Effects {
        apply::<()>(state, Event::key(KeyPress::char(key)))
    }

    fn identity(pid: u32, start_key: u64) -> ProcessIdentity {
        ProcessIdentity::new(pid, start_key)
    }

    // ------------------------------------------------------- state plus action

    #[test]
    fn quitting_sets_the_flag_and_asks_for_shutdown() {
        let mut state = state();

        let effects = reduce(&mut state, Action::Quit);

        assert!(state.should_quit());
        assert_eq!(effects.as_slice(), &[Effect::Shutdown]);
    }

    #[test]
    fn a_forced_refresh_asks_the_sampler_and_touches_nothing_else() {
        let mut state = running();
        let before = state.snapshot().map(Arc::as_ptr);

        let effects = reduce(&mut state, Action::ForceRefresh);

        assert_eq!(effects.as_slice(), &[Effect::RequestSample]);
        assert_eq!(state.snapshot().map(Arc::as_ptr), before);
    }

    #[test]
    fn cycling_the_theme_and_glyph_mode_only_redraws() {
        let mut state = state();
        let theme = state.display().theme;

        let effects = reduce(&mut state, Action::CycleTheme);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_ne!(state.display().theme, theme);
        assert_eq!(state.display().theme, ThemeId::default().next());

        let glyphs = state.display().glyph_mode;
        let _ = reduce(&mut state, Action::CycleGlyphMode);
        assert_ne!(state.display().glyph_mode, glyphs);
    }

    #[test]
    fn changing_to_the_current_view_produces_no_effects() {
        let mut state = state();

        assert!(
            reduce(&mut state, Action::ChangeView(ViewId::Overview)).is_empty(),
            "§16.1: no change means no redraw"
        );
        assert_eq!(
            reduce(&mut state, Action::ChangeView(ViewId::Processes)).as_slice(),
            &[Effect::RequestRedraw]
        );
        assert_eq!(state.view(), ViewId::Processes);
    }

    #[test]
    fn no_reducer_action_performs_input_or_output() {
        // §17.4 asks for "no direct OS operation". The structural proof is that the
        // only effects that touch anything outside this process are the ones the
        // reducer *returns*, and the destructive ones are unreachable without a
        // confirmed pending action. Walk every action on a fresh state and assert
        // that nothing escapes except redraws, sample requests and detail fetches.
        let allowed = |effect: &Effect| {
            matches!(
                effect,
                Effect::RequestRedraw
                    | Effect::RequestSample
                    | Effect::FetchProcessDetail(_)
                    | Effect::Shutdown
            )
        };
        for action in every_action() {
            let mut state = running();
            let effects = reduce(&mut state, action.clone());
            assert!(
                effects.iter().all(allowed),
                "{action:?} produced {effects:?}"
            );
        }
    }

    /// Every action, with plausible payloads. Used by the safety sweeps.
    fn every_action() -> Vec<Action> {
        vec![
            Action::Quit,
            Action::ForceRefresh,
            Action::CycleTheme,
            Action::CycleGlyphMode,
            Action::ToggleHelp,
            Action::OpenCommandPalette,
            Action::NextPanel,
            Action::PreviousPanel,
            Action::ChangeView(ViewId::Processes),
            Action::TogglePause,
            Action::SeekHistory(Seek::step_back()),
            Action::SeekHistory(Seek::Oldest),
            Action::ReturnLive,
            Action::SelectNext,
            Action::SelectPrevious,
            Action::SelectPageDown,
            Action::SelectPageUp,
            Action::SelectFirst,
            Action::SelectLast,
            Action::InspectSelected,
            Action::BeginFilterEdit,
            Action::SetFilter("rustc".to_owned()),
            Action::NextMatch,
            Action::PreviousMatch,
            Action::OpenSortSelector,
            Action::SetSort(SortField::MemoryShare),
            Action::ReverseSort,
            Action::ToggleTreeView,
            Action::PinSelected,
            Action::Pin(identity(1, 1)),
            Action::RequestProcessDetail(identity(31_842, 900_100)),
            Action::OpenSignalDialog,
            Action::ProposeSignal(SignalKind::Term),
            Action::ProposeSignal(SignalKind::Kill),
            Action::ProposeRenice,
            Action::RequestSignal(identity(31_842, 900_100), SignalKind::Kill),
            Action::ConfirmPendingAction,
            Action::ConfirmForcefulAction,
            Action::CancelOverlay,
            Action::InsertChar('x'),
            Action::DeleteBackward,
            Action::DeleteForward,
            Action::DeleteWordBackward,
            Action::ClearInput,
            Action::MoveCursorLeft,
            Action::MoveCursorRight,
            Action::MoveCursorToStart,
            Action::MoveCursorToEnd,
            Action::SubmitInput,
        ]
    }

    // ---------------------------------------------------------- stable selection

    #[test]
    fn the_selection_is_retained_across_a_new_snapshot() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectNext);
        let selected = state.selected().expect("a row is selected");
        assert_eq!(selected, identity(1_221, 700_050));

        // The table re-sorts: postgres is now the busiest process.
        let hotter = vec![
            Fake::new(31_842, 900_100, "rustc").cpu(10.0),
            Fake::new(1_221, 700_050, "postgres").cpu(300.0),
            Fake::new(507, 100_010, "WindowServer").cpu(21.0),
            Fake::new(9_182, 850_300, "node").cpu(12.0),
            Fake::new(1, 1, "launchd").cpu(0.1),
        ];
        deliver(&mut state, 2, &hotter);

        assert_eq!(
            state.selected(),
            Some(selected),
            "§7.2: selection follows the process across a refresh"
        );
        assert_eq!(state.selected_row(), Some(0), "it moved to the top row");
    }

    #[test]
    fn an_exiting_selected_process_hands_over_to_the_nearest_row() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectNext);
        assert_eq!(state.selected(), Some(identity(1_221, 700_050)));

        let mut exiting = table();
        exiting[1] = Fake::new(1_221, 700_050, "postgres")
            .cpu(54.0)
            .exiting_at(2);
        deliver(&mut state, 2, &exiting);

        assert_ne!(state.selected(), Some(identity(1_221, 700_050)));
        assert_eq!(
            state.selected_row(),
            Some(1),
            "§7.2: never reset to the top when the selected process exits"
        );
        assert_eq!(state.selected(), Some(identity(507, 100_010)));
    }

    #[test]
    fn a_reused_pid_does_not_inherit_the_selection() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        assert_eq!(state.selected(), Some(identity(31_842, 900_100)));

        let mut recycled = table();
        recycled[0] = Fake::new(31_842, 900_100, "rustc")
            .cpu(287.0)
            .exiting_at(2)
            .reused_as(977_400);
        deliver(&mut state, 2, &recycled);

        assert_ne!(
            state.selected(),
            Some(identity(31_842, 977_400)),
            "§26: a reused PID is a different process"
        );
    }

    #[test]
    fn re_sorting_and_filtering_keep_the_selected_process() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectLast);
        let selected = state.selected().expect("selected");

        let _ = reduce(&mut state, Action::ReverseSort);
        assert_eq!(state.selected(), Some(selected));

        let _ = reduce(&mut state, Action::ToggleTreeView);
        assert_eq!(state.selected(), Some(selected));
    }

    #[test]
    fn filtering_the_selected_process_away_moves_to_a_surviving_row() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        assert_eq!(state.selected(), Some(identity(31_842, 900_100)));

        let effects = reduce(&mut state, Action::SetFilter("postgres".to_owned()));

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.rows().len(), 1);
        assert_eq!(state.selected(), Some(identity(1_221, 700_050)));
    }

    #[test]
    fn selection_movement_clamps_and_stops_redrawing_at_the_ends() {
        let mut state = running();

        assert!(reduce(&mut state, Action::SelectPrevious).is_empty());
        let _ = reduce(&mut state, Action::SelectLast);
        assert!(reduce(&mut state, Action::SelectNext).is_empty());
        assert_eq!(state.selected_row(), Some(state.rows().len() - 1));

        let _ = reduce(&mut state, Action::SelectPageUp);
        assert_eq!(state.selected_row(), Some(0));
    }

    // ------------------------------------------------------------------- pins

    #[test]
    fn pinning_is_by_identity_and_a_reused_pid_inherits_nothing() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let pinned = state.selected().expect("selected");

        let effects = reduce(&mut state, Action::PinSelected);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert!(state.is_pinned(pinned));

        let mut recycled = table();
        recycled[0] = Fake::new(31_842, 900_100, "rustc")
            .cpu(287.0)
            .exiting_at(2)
            .reused_as(977_400);
        deliver(&mut state, 2, &recycled);

        assert!(
            !state.is_pinned(identity(31_842, 977_400)),
            "§2.5: PID reuse must not attach a pin to the wrong process"
        );
        assert!(
            state.is_pinned(pinned),
            "pins are session-local and explicit; the old identity stays pinned"
        );
    }

    #[test]
    fn pinning_the_same_process_twice_unpins_it() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);

        let _ = reduce(&mut state, Action::PinSelected);
        assert_eq!(state.pins().len(), 1);
        let _ = reduce(&mut state, Action::PinSelected);
        assert!(state.pins().is_empty());
    }

    #[test]
    fn the_pin_list_is_bounded_and_says_so() {
        let mut state = running();
        let attempts = u32::try_from(MAX_PINNED_PROCESSES).expect("a small bound") + 4;
        for pid in 0..attempts {
            let _ = reduce(&mut state, Action::Pin(identity(pid, u64::from(pid))));
        }

        assert_eq!(state.pins().len(), MAX_PINNED_PROCESSES);
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("pinned")),
            "the refusal is explained"
        );
    }

    #[test]
    fn pinning_with_nothing_selected_does_nothing() {
        let mut state = state();
        assert!(reduce(&mut state, Action::PinSelected).is_empty());
        assert!(state.pins().is_empty());
    }

    // -------------------------------------------------------------- time lens

    #[test]
    fn space_freezes_the_display_while_collection_continues() {
        let mut state = running();
        let frozen = state.snapshot().cloned().expect("a snapshot");

        let effects = reduce(&mut state, Action::TogglePause);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.timeline_status().label(), "PAUSED");

        deliver(&mut state, 2, &table());

        assert!(
            Arc::ptr_eq(state.snapshot().expect("still frozen"), &frozen),
            "§2.1: pausing freezes the visible timeline"
        );
        assert_eq!(
            state.live_snapshot().map(|snapshot| snapshot.sequence),
            Some(2),
            "collection continues"
        );
        assert_eq!(state.history().len(), 2, "history keeps filling");
    }

    #[test]
    fn pausing_before_the_first_snapshot_does_not_freeze_a_blank_screen() {
        let mut state = state();
        let _ = reduce(&mut state, Action::TogglePause);
        assert!(state.timeline().is_paused());

        deliver(&mut state, 1, &table());

        assert!(
            state.snapshot().is_some(),
            "there was nothing to freeze, so the first sample fills the view"
        );
        assert_eq!(state.rows().len(), 5);
        assert_eq!(state.timeline_status().label(), "PAUSED");

        // From now on the display really is frozen.
        deliver(&mut state, 2, &table());
        assert_eq!(state.snapshot().map(|snapshot| snapshot.sequence), Some(1));
    }

    #[test]
    fn seeking_enters_history_and_the_header_is_unmistakable() {
        let mut state = running();
        deliver(&mut state, 2, &table());
        deliver(&mut state, 3, &table());

        let effects = reduce(&mut state, Action::SeekHistory(Seek::step_back()));

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        let status = state.timeline_status();
        assert!(matches!(status, TimelineStatus::History { .. }));
        assert_eq!(status.label(), "HISTORY -00:01");
        assert_eq!(status.symbol(), '<');
        assert_ne!(
            status.token(),
            TimelineStatus::Live.token(),
            "§26: historical state must be visually unmistakable"
        );
        assert!(!state.allows_process_actions());
    }

    #[test]
    fn seeking_with_no_history_explains_itself_instead_of_entering_history() {
        let mut state = state();

        let effects = reduce(&mut state, Action::SeekHistory(Seek::step_back()));

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert!(state.timeline().is_live());
        assert_eq!(
            state.notices().last().map(|notice| notice.kind),
            Some(NoticeKind::Interaction)
        );
        assert_eq!(state.timeline().last_seek(), None);
    }

    #[test]
    fn a_zero_distance_seek_produces_nothing_at_all() {
        let mut state = running();
        assert!(reduce(&mut state, Action::SeekHistory(Seek::Forward(0))).is_empty());
    }

    #[test]
    fn returning_to_live_catches_the_display_up() {
        let mut state = running();
        let _ = reduce(&mut state, Action::TogglePause);
        deliver(&mut state, 2, &table());
        deliver(&mut state, 3, &table());
        let _ = reduce(&mut state, Action::SeekHistory(Seek::Backward(2)));
        assert!(state.timeline().is_historical());

        let effects = reduce(&mut state, Action::ReturnLive);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.timeline_status(), TimelineStatus::Live);
        assert_eq!(
            state.snapshot().map(|snapshot| snapshot.sequence),
            Some(3),
            "the display caught up to live"
        );
        assert!(state.allows_process_actions());
        assert!(
            reduce(&mut state, Action::ReturnLive).is_empty(),
            "already live"
        );
    }

    #[test]
    fn space_resumes_from_history() {
        let mut state = running();
        deliver(&mut state, 2, &table());
        let _ = reduce(&mut state, Action::SeekHistory(Seek::step_back()));

        let _ = reduce(&mut state, Action::TogglePause);

        assert_eq!(state.timeline_status(), TimelineStatus::Live);
        assert_eq!(state.snapshot().map(|snapshot| snapshot.sequence), Some(2));
    }

    #[test]
    fn the_selection_survives_a_pause_and_a_return_to_live() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectNext);
        let selected = state.selected().expect("selected");

        let _ = reduce(&mut state, Action::TogglePause);
        deliver(&mut state, 2, &table());
        let _ = reduce(&mut state, Action::ReturnLive);

        assert_eq!(state.selected(), Some(selected));
    }

    // ------------------------------------------------- history blocks actions

    #[test]
    fn every_destructive_action_is_refused_away_from_live() {
        let destructive = [
            Action::OpenSignalDialog,
            Action::ProposeSignal(SignalKind::Term),
            Action::ProposeSignal(SignalKind::Kill),
            Action::ProposeRenice,
            Action::RequestSignal(identity(31_842, 900_100), SignalKind::Kill),
            Action::ConfirmPendingAction,
            Action::ConfirmForcefulAction,
        ];

        for frozen_by in [Action::TogglePause, Action::SeekHistory(Seek::step_back())] {
            for action in &destructive {
                let mut state = running();
                deliver(&mut state, 2, &table());
                let _ = reduce(&mut state, frozen_by.clone());
                assert!(!state.allows_process_actions(), "{frozen_by:?}");
                let _ = reduce(&mut state, Action::SelectFirst);

                let effects = reduce(&mut state, action.clone());

                assert!(
                    !effects.touches_a_process(),
                    "{action:?} produced {effects:?} while not live (§15.1)"
                );
                assert!(state.pending_process_action().is_none());
                assert!(
                    state
                        .notices()
                        .iter()
                        .any(|notice| notice.message.contains("not live")),
                    "the refusal is explained"
                );
            }
        }
    }

    #[test]
    fn non_destructive_actions_still_work_in_history() {
        let mut state = running();
        deliver(&mut state, 2, &table());
        let _ = reduce(&mut state, Action::SeekHistory(Seek::step_back()));

        assert_eq!(
            reduce(&mut state, Action::SelectNext).as_slice(),
            &[Effect::RequestRedraw],
            "browsing the frozen table is not a process action"
        );
        assert_eq!(
            reduce(&mut state, Action::ChangeView(ViewId::Inspect)).as_slice(),
            &[Effect::RequestRedraw]
        );
        assert_eq!(
            reduce(&mut state, Action::Quit).as_slice(),
            &[Effect::Shutdown]
        );
    }

    // -------------------------------------------------------- the confirm chain

    #[test]
    fn no_single_action_yields_a_signal_effect() {
        // §15.1, §17.4: the whole point of the chain. Every action, applied to a
        // state that is live and has a selection, on its own.
        for action in every_action() {
            let mut state = running();
            let _ = reduce(&mut state, Action::SelectFirst);

            let effects = reduce(&mut state, action.clone());

            assert!(
                !effects.touches_a_process(),
                "{action:?} produced {effects:?} without a confirmation"
            );
        }
    }

    #[test]
    fn proposing_a_signal_opens_a_confirmation_carrying_the_full_identity() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let selected = state.selected().expect("selected");

        let effects = reduce(&mut state, Action::ProposeSignal(SignalKind::Term));

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.input_mode(), InputMode::ConfirmProcessAction);
        assert_eq!(
            state.pending_process_action(),
            Some(PendingProcessAction::Signal {
                identity: selected,
                signal: SignalKind::Term,
            }),
            "§17.4: the PID identity is attached to the pending action"
        );
        assert_eq!(
            state
                .pending_process_action()
                .map(|pending| pending.identity().start_key),
            Some(900_100),
            "the start key travels with it, not just the PID"
        );
    }

    #[test]
    fn confirming_a_term_emits_exactly_one_signal_effect() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let selected = state.selected().expect("selected");
        let _ = reduce(&mut state, Action::ProposeSignal(SignalKind::Term));

        let effects = reduce(&mut state, Action::ConfirmPendingAction);

        assert_eq!(
            effects.as_slice(),
            &[
                Effect::SignalProcess {
                    identity: selected,
                    signal: SignalKind::Term,
                },
                Effect::RequestRedraw,
            ]
        );
        assert!(state.pending_process_action().is_none());
        assert!(state.overlays().is_empty());
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("SIGTERM")),
            "the request is reported (§15.1)"
        );
    }

    #[test]
    fn sigkill_refuses_an_ordinary_confirmation_and_keeps_the_dialog_open() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let _ = reduce(&mut state, Action::ProposeSignal(SignalKind::Kill));

        let effects = reduce(&mut state, Action::ConfirmPendingAction);

        assert!(!effects.touches_a_process(), "§15.1: Enter is not enough");
        assert!(state.pending_process_action().is_some());
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("forceful")),
        );

        let effects = reduce(&mut state, Action::ConfirmForcefulAction);

        assert!(effects.touches_a_process());
        assert!(state.pending_process_action().is_none());
    }

    #[test]
    fn the_signal_dialog_chooses_then_confirms() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);

        let effects = reduce(&mut state, Action::OpenSignalDialog);
        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(
            state.process_action_stage(),
            Some(ProcessActionStage::ChooseSignal {
                identity: identity(31_842, 900_100),
                cursor: 0,
            })
        );
        assert!(
            state.pending_process_action().is_none(),
            "choosing is not yet pending"
        );

        // Walk down to SIGKILL with the keys the confirm mode actually binds, then
        // pick it: still only a proposal.
        for _ in 0..3 {
            let _ = reduce(&mut state, Action::SelectNext);
        }
        assert_eq!(
            state
                .process_action_stage()
                .and_then(|stage| stage.highlighted_signal()),
            Some(SignalKind::Kill)
        );
        let effects = reduce(&mut state, Action::ConfirmPendingAction);
        assert!(!effects.touches_a_process());
        assert_eq!(
            state.pending_process_action(),
            Some(PendingProcessAction::Signal {
                identity: identity(31_842, 900_100),
                signal: SignalKind::Kill,
            })
        );

        // And the forceful key is still required for the second stage.
        assert!(!reduce(&mut state, Action::ConfirmPendingAction).touches_a_process());
        assert!(reduce(&mut state, Action::ConfirmForcefulAction).touches_a_process());
    }

    #[test]
    fn cancelling_a_confirmation_leaves_no_pending_action_and_no_effect() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let _ = reduce(&mut state, Action::ProposeSignal(SignalKind::Kill));

        let effects = reduce(&mut state, Action::CancelOverlay);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert!(state.pending_process_action().is_none());
        assert!(state.overlays().is_empty());
        assert_eq!(state.input_mode(), InputMode::Normal);

        // And a confirmation afterwards is inert.
        assert!(reduce(&mut state, Action::ConfirmForcefulAction).is_empty());
    }

    #[test]
    fn a_confirmation_with_nothing_pending_is_inert() {
        let mut state = running();
        assert!(reduce(&mut state, Action::ConfirmPendingAction).is_empty());
        assert!(reduce(&mut state, Action::ConfirmForcefulAction).is_empty());
    }

    #[test]
    fn a_request_signal_action_opens_a_dialog_rather_than_signalling() {
        let mut state = running();

        let effects = reduce(
            &mut state,
            Action::RequestSignal(identity(31_842, 900_100), SignalKind::Kill),
        );

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(
            state.pending_process_action(),
            Some(PendingProcessAction::Signal {
                identity: identity(31_842, 900_100),
                signal: SignalKind::Kill,
            })
        );
    }

    #[test]
    fn a_pending_action_is_cancelled_when_its_process_exits() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let _ = reduce(&mut state, Action::ProposeSignal(SignalKind::Term));
        assert!(state.pending_process_action().is_some());

        let mut exiting = table();
        exiting[0] = Fake::new(31_842, 900_100, "rustc").cpu(287.0).exiting_at(2);
        deliver(&mut state, 2, &exiting);

        assert!(
            state.pending_process_action().is_none(),
            "§15.1: already-exited processes are reported, not signalled"
        );
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("already exited")),
        );
    }

    #[test]
    fn a_pending_action_is_cancelled_when_the_pid_is_reused() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let _ = reduce(&mut state, Action::ProposeSignal(SignalKind::Term));

        let mut recycled = table();
        recycled[0] = Fake::new(31_842, 900_100, "rustc")
            .cpu(287.0)
            .exiting_at(2)
            .reused_as(977_400);
        deliver(&mut state, 2, &recycled);

        assert!(state.pending_process_action().is_none());
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("different process")),
            "§26: PID reuse is reported"
        );
    }

    #[test]
    fn signalling_an_already_exited_process_is_refused_before_the_dialog_opens() {
        let mut state = state();
        let zombie = vec![
            Fake::new(4_242, 424_200, "orphan")
                .cpu(0.0)
                .state(ProcessState::Zombie),
        ];
        deliver(&mut state, 1, &zombie);
        let _ = reduce(&mut state, Action::SelectFirst);

        let effects = reduce(&mut state, Action::ProposeSignal(SignalKind::Term));

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert!(state.pending_process_action().is_none());
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("zombie")),
        );
    }

    #[test]
    fn process_control_is_refused_when_the_platform_cannot_do_it() {
        let mut state = state();
        let capabilities = CapabilitySnapshot {
            process_signals: CapabilityState::PermissionDenied,
            renice: CapabilityState::Unsupported,
            ..all_capabilities()
        };
        let snapshot = Arc::new(snapshot_with(1, &table(), capabilities));
        let at = snapshot.captured_at;
        let _ = apply::<()>(&mut state, Event::Snapshot(snapshot));
        state.record_render(at, Duration::from_millis(1));
        let _ = reduce(&mut state, Action::SelectFirst);

        let _ = reduce(&mut state, Action::OpenSignalDialog);
        assert!(state.process_action_stage().is_none());
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.kind == NoticeKind::Permission),
            "§4: a permission problem suggests what privileges would provide"
        );

        let _ = reduce(&mut state, Action::ProposeRenice);
        assert!(state.process_action_stage().is_none());
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("not supported")),
        );
    }

    #[test]
    fn the_renice_dialog_adjusts_a_value_before_confirming() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);

        let _ = reduce(&mut state, Action::ProposeRenice);
        assert_eq!(
            state.process_action_stage(),
            Some(ProcessActionStage::ChooseNice {
                identity: identity(31_842, 900_100),
                nice: 0,
            })
        );

        let _ = reduce(&mut state, Action::SelectNext);
        let _ = reduce(&mut state, Action::SelectNext);
        let _ = reduce(&mut state, Action::ConfirmPendingAction);
        assert_eq!(
            state.pending_process_action(),
            Some(PendingProcessAction::Renice {
                identity: identity(31_842, 900_100),
                nice: 2,
            })
        );

        let effects = reduce(&mut state, Action::ConfirmPendingAction);
        assert_eq!(
            effects.as_slice(),
            &[
                Effect::ReniceProcess {
                    identity: identity(31_842, 900_100),
                    nice: 2,
                },
                Effect::RequestRedraw,
            ]
        );
    }

    // ------------------------------------------------------- pressure alerts

    /// A state that rings the bell, as `diagnostics.bell_on_critical = true` builds it.
    fn ringing() -> AppState {
        AppState::new(AppSettings {
            started_at: epoch(),
            size: (160, 48),
            bell_on_critical: true,
            ..AppSettings::default()
        })
    }

    /// Delivers a snapshot whose radar reports `pressure` for `id`.
    fn deliver_pressure(
        state: &mut AppState,
        sequence: u64,
        id: PressureId,
        pressure: MetricState<PressureState>,
    ) -> Effects {
        let snapshot = arc_snapshot_with_pressure(sequence, &table(), id, pressure);
        let at = snapshot.captured_at;
        let effects = apply::<()>(state, Event::Snapshot(snapshot));
        state.record_render(at, Duration::from_millis(4));
        effects
    }

    /// Every pressure notice recorded so far.
    fn pressure_notices(state: &AppState) -> Vec<&Notice> {
        state
            .notices()
            .iter()
            .filter(|notice| notice.kind == NoticeKind::Pressure)
            .collect()
    }

    #[test]
    fn an_escalating_signal_is_announced_once_and_not_once_a_second() {
        let mut state = state();

        let _ = deliver_pressure(
            &mut state,
            1,
            PressureId::Cpu,
            MetricState::Available(PressureState::Critical),
        );
        assert_eq!(pressure_notices(&state).len(), 1);

        // The radar keeps reporting critical for as long as the CPU is saturated.
        for sequence in 2..30 {
            let _ = deliver_pressure(
                &mut state,
                sequence,
                PressureId::Cpu,
                MetricState::Available(PressureState::Critical),
            );
        }

        let notices = pressure_notices(&state);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(
            notices.first().map(|notice| notice.occurrences),
            Some(1),
            "a repeat count here would mean the transition was detected again"
        );
        let message = notices.first().map(|notice| notice.message.as_str());
        assert!(
            message.is_some_and(|message| message.starts_with("CPU is now critical")),
            "{message:?}"
        );
        assert!(
            message.is_some_and(|message| message.contains("diagnostics.cpu_watch_percent")),
            "§2.3: the notice quotes the rule that derived the state: {message:?}"
        );
    }

    #[test]
    fn a_signal_that_goes_unavailable_is_not_reported_as_a_recovery() {
        // §4/§26: "the OS refused the read" must never reach the user as good news.
        let mut state = state();
        let _ = deliver_pressure(
            &mut state,
            1,
            PressureId::Cpu,
            MetricState::Available(PressureState::Critical),
        );
        let before = pressure_notices(&state).len();

        let _ = deliver_pressure(
            &mut state,
            2,
            PressureId::Cpu,
            MetricState::PermissionDenied,
        );

        assert_eq!(
            pressure_notices(&state).len(),
            before,
            "an unavailable signal said something: {:?}",
            pressure_notices(&state)
        );
        assert_eq!(state.pressure_watch().last_state(PressureId::Cpu), None);
    }

    #[test]
    fn a_recovery_is_reported_at_a_lower_severity_than_the_escalation() {
        let mut state = state();
        let _ = deliver_pressure(
            &mut state,
            1,
            PressureId::Memory,
            MetricState::Available(PressureState::Critical),
        );
        let _ = deliver_pressure(
            &mut state,
            2,
            PressureId::Memory,
            MetricState::Available(PressureState::Normal),
        );

        let severities: Vec<Severity> = pressure_notices(&state)
            .iter()
            .map(|notice| notice.severity)
            .collect();
        assert_eq!(severities, vec![Severity::Critical, Severity::Info]);
    }

    #[test]
    fn the_bell_is_silent_unless_configured_and_unless_critical() {
        // Nothing configured: no bell, whatever the radar says.
        let mut quiet = state();
        let effects = deliver_pressure(
            &mut quiet,
            1,
            PressureId::Cpu,
            MetricState::Available(PressureState::Critical),
        );
        assert!(
            !effects.contains(&Effect::RingBell),
            "the bell is off by default: {effects:?}"
        );
        assert_eq!(
            pressure_notices(&quiet).len(),
            1,
            "the notice still happens"
        );

        // Configured, but only `watch`: still silent.
        let mut ringing = ringing();
        let effects = deliver_pressure(
            &mut ringing,
            1,
            PressureId::Cpu,
            MetricState::Available(PressureState::Watch),
        );
        assert!(
            !effects.contains(&Effect::RingBell),
            "§2.3: only critical rings: {effects:?}"
        );

        // Configured and critical: one bell.
        let effects = deliver_pressure(
            &mut ringing,
            2,
            PressureId::Cpu,
            MetricState::Available(PressureState::Critical),
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == Effect::RingBell)
                .count(),
            1,
            "{effects:?}"
        );

        // Still critical, and on the way back down: silent again.
        for (sequence, pressure) in [
            (3, MetricState::Available(PressureState::Critical)),
            (4, MetricState::Available(PressureState::Watch)),
            (5, MetricState::Available(PressureState::Normal)),
        ] {
            let effects = deliver_pressure(&mut ringing, sequence, PressureId::Cpu, pressure);
            assert!(
                !effects.contains(&Effect::RingBell),
                "sequence {sequence} rang: {effects:?}"
            );
        }
    }

    #[test]
    fn two_signals_escalating_together_ring_once() {
        let mut state = ringing();
        let mut snapshot = snapshot_of(1, &table());
        for id in [PressureId::Memory, PressureId::Swap] {
            set_pressure(
                &mut snapshot,
                id,
                MetricState::Available(PressureState::Critical),
            );
        }

        let effects = apply::<()>(&mut state, Event::Snapshot(Arc::new(snapshot)));

        assert_eq!(pressure_notices(&state).len(), 2, "both are worth saying");
        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == Effect::RingBell)
                .count(),
            1,
            "two beeps say nothing the first did not: {effects:?}"
        );
    }

    #[test]
    fn alerts_still_fire_while_the_timeline_is_frozen() {
        // §2.1 freezes the *displayed* snapshot; collection continues, and so does
        // the machine. A user who paused to read one spike must still hear about the
        // next — and the notice is dialog-free, so it cannot disturb their scrubbing.
        let mut state = ringing();
        let _ = deliver_pressure(
            &mut state,
            1,
            PressureId::Cpu,
            MetricState::Available(PressureState::Normal),
        );
        let _ = reduce(&mut state, Action::TogglePause);
        assert!(!state.timeline().is_live());
        let frozen = state.snapshot().map(|snapshot| snapshot.sequence);

        let effects = deliver_pressure(
            &mut state,
            2,
            PressureId::Cpu,
            MetricState::Available(PressureState::Critical),
        );

        assert_eq!(pressure_notices(&state).len(), 1);
        assert!(effects.contains(&Effect::RingBell));
        assert_eq!(
            state.snapshot().map(|snapshot| snapshot.sequence),
            frozen,
            "the alert must not drag the frozen view forward"
        );
    }

    #[test]
    fn a_dropped_snapshot_cannot_produce_an_alert() {
        // A re-delivered or reordered snapshot is discarded before anything reads it
        // (§10.3), so it must not be able to re-announce a transition either.
        let mut state = state();
        let _ = deliver_pressure(
            &mut state,
            2,
            PressureId::Cpu,
            MetricState::Available(PressureState::Critical),
        );
        assert_eq!(pressure_notices(&state).len(), 1);

        let stale = arc_snapshot_with_pressure(
            1,
            &table(),
            PressureId::Cpu,
            MetricState::Available(PressureState::Normal),
        );
        let effects = apply::<()>(&mut state, Event::Snapshot(stale));

        assert!(effects.is_empty());
        assert_eq!(pressure_notices(&state).len(), 1);
        assert_eq!(
            state.pressure_watch().last_state(PressureId::Cpu),
            Some(PressureState::Critical)
        );
    }

    // ------------------------------------------------------------- coalescing

    #[test]
    fn a_snapshot_that_supersedes_an_unrendered_one_is_counted_as_coalesced() {
        let mut state = state();

        let _ = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(1, &table())));
        assert!(state.has_unrendered_snapshot());
        let _ = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(2, &table())));
        let _ = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(3, &table())));

        assert_eq!(
            state.health().coalesced_samples,
            2,
            "§10.3: two snapshots were superseded before the UI drew"
        );
        assert_eq!(
            state.live_snapshot().map(|snapshot| snapshot.sequence),
            Some(3),
            "the newest is kept"
        );
        assert_eq!(state.history().len(), 3, "no sample is lost from history");
    }

    #[test]
    fn an_older_snapshot_is_dropped_rather_than_queued() {
        let mut state = running();
        assert_eq!(state.live_snapshot().map(|s| s.sequence), Some(1));

        let effects = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(0, &table())));

        assert!(effects.is_empty(), "an old sample produces no work");
        assert_eq!(state.live_snapshot().map(|s| s.sequence), Some(1));
        assert_eq!(state.health().coalesced_samples, 0);
    }

    #[test]
    fn a_re_delivered_snapshot_is_dropped() {
        let mut state = running();
        let effects = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(1, &table())));
        assert!(effects.is_empty());
        assert_eq!(state.history().len(), 1);
    }

    #[test]
    fn a_health_update_never_erases_the_coalescing_count() {
        let mut state = state();
        let _ = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(1, &table())));
        let _ = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(2, &table())));
        assert_eq!(state.health().coalesced_samples, 1);

        let effects = apply::<()>(
            &mut state,
            Event::health(CollectorHealth {
                dropped_samples: 3,
                ..CollectorHealth::default()
            }),
        );

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.health().dropped_samples, 3);
        assert_eq!(state.health().coalesced_samples, 1);
    }

    #[test]
    fn effects_deduplicate_redraw_requests() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        // Inspect both closes the sort selector and applies a sort: two redraws
        // collapse into one.
        let _ = reduce(&mut state, Action::OpenSortSelector);
        let effects = reduce(&mut state, Action::InspectSelected);

        assert_eq!(
            effects
                .iter()
                .filter(|effect| **effect == Effect::RequestRedraw)
                .count(),
            1
        );
    }

    // ------------------------------------------------------------------ events

    #[test]
    fn a_resize_revalidates_focus_and_always_redraws() {
        let mut state = running();
        state.focus = PanelFocus::Pins;

        let effects = apply::<()>(
            &mut state,
            Event::Terminal(TerminalEvent::Resize {
                columns: 90,
                rows: 24,
            }),
        );

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.size(), (90, 24));
        assert!(state.focus().is_present(&state.layout()));
    }

    #[test]
    fn mouse_and_focus_events_are_ignored() {
        let mut state = running();
        for event in [
            Event::<()>::Terminal(TerminalEvent::FocusGained),
            Event::<()>::Terminal(TerminalEvent::FocusLost),
            Event::<()>::Terminal(TerminalEvent::Mouse(crate::event::MouseInput {
                action: crate::event::MouseAction::ScrollDown,
                column: 3,
                row: 3,
                modifiers: crate::event::Modifiers::NONE,
            })),
        ] {
            assert!(apply(&mut state, event).is_empty());
        }
    }

    #[test]
    fn a_tick_releases_a_held_sequence_prefix() {
        // §6.2 binds both `g` and `gg`, so `g` is held. Without `poll_timeout` in
        // the tick handler the glyph mode would not cycle until the next keypress.
        let mut state = running();
        let glyphs = state.display().glyph_mode;

        let effects = press(&mut state, 'g');
        assert!(
            effects.is_empty(),
            "the prefix is held, nothing happens yet"
        );
        assert_eq!(state.display().glyph_mode, glyphs);

        let later = state.clock() + crate::keymap::DEFAULT_SEQUENCE_TIMEOUT * 2;
        let effects = apply::<()>(&mut state, Event::Tick(later));

        assert!(effects.contains(&Effect::RequestRedraw));
        assert_ne!(
            state.display().glyph_mode,
            glyphs,
            "the timeout released `g`'s own action"
        );
    }

    #[test]
    fn a_mode_change_that_was_not_a_keypress_drops_a_held_prefix() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let selected = state.selected().expect("selected");
        let _ = reduce(&mut state, Action::InspectSelected);
        assert_eq!(state.input_mode(), InputMode::ProcessDetail);

        // `gg` is bound in the detail overlay, so a lone `g` is held as a prefix.
        let _ = press(&mut state, 'g');
        assert!(state.resolver.has_pending_sequence());

        // The process exits: the overlay closes, so the mode changes without a key.
        let _ = apply::<()>(
            &mut state,
            Event::Detail(ProcessDetailResult::Vanished(selected)),
        );

        assert_eq!(state.input_mode(), InputMode::Normal);
        assert!(
            !state.resolver.has_pending_sequence(),
            "a half-typed sequence must not complete against another mode's table"
        );
        let later = state.clock() + crate::keymap::DEFAULT_SEQUENCE_TIMEOUT * 2;
        let _ = apply::<()>(&mut state, Event::Tick(later));
        assert_eq!(
            state.display().glyph_mode,
            crate::glyphs::GlyphMode::default(),
            "and it is not replayed as `g` in the new mode either"
        );
    }

    #[test]
    fn a_second_g_completes_the_sequence_instead_of_cycling_glyphs() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectLast);
        let glyphs = state.display().glyph_mode;

        let _ = press(&mut state, 'g');
        let _ = press(&mut state, 'g');

        assert_eq!(state.selected_row(), Some(0), "`gg` is first row (§6.2)");
        assert_eq!(state.display().glyph_mode, glyphs);
    }

    #[test]
    fn an_idle_tick_redraws_at_most_once_a_second() {
        let mut state = running();
        let base = state.clock();
        state.record_render(base, Duration::from_millis(3));

        assert!(
            apply::<()>(&mut state, Event::Tick(base + Duration::from_millis(100))).is_empty(),
            "§16.1: no redraw busy loop"
        );
        assert_eq!(
            apply::<()>(&mut state, Event::Tick(base + IDLE_REDRAW_INTERVAL)).as_slice(),
            &[Effect::RequestRedraw],
            "the header clock still has to advance"
        );
    }

    #[test]
    fn a_tick_before_the_first_frame_asks_for_one() {
        let mut state = state();
        let now = state.clock();
        let effects = apply::<()>(&mut state, Event::Tick(now));
        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
    }

    #[test]
    fn a_key_press_is_resolved_in_the_current_mode() {
        let mut state = running();

        let _ = press(&mut state, '/');
        assert_eq!(state.input_mode(), InputMode::FilterEdit);

        // `q` types a literal `q` while editing (§6.2's "unless editing text").
        let _ = press(&mut state, 'q');
        assert!(!state.should_quit());
        assert_eq!(
            state
                .top_overlay()
                .and_then(Overlay::text_input)
                .map(TextInput::text),
            Some("q")
        );

        let _ = apply::<()>(&mut state, Event::key(KeyPress::plain(Key::Enter)));
        assert_eq!(state.filter_text(), "q");
        assert_eq!(state.input_mode(), InputMode::Normal);
    }

    #[test]
    fn escape_discards_a_half_typed_filter() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SetFilter("rustc".to_owned()));
        let _ = reduce(&mut state, Action::BeginFilterEdit);
        let _ = reduce(&mut state, Action::ClearInput);
        let _ = reduce(&mut state, Action::InsertChar('z'));

        let _ = reduce(&mut state, Action::CancelOverlay);

        assert_eq!(
            state.filter_text(),
            "rustc",
            "a half-typed filter never hides rows"
        );
        assert!(state.overlays().is_empty());
    }

    #[test]
    fn a_config_reload_event_only_redraws_because_the_payload_is_opaque() {
        let mut state = running();
        let effects = apply(&mut state, Event::ConfigReloaded(()));
        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert!(
            state.notices().is_empty(),
            "the runtime reports the outcome"
        );
    }

    // ------------------------------------------------------------------ detail

    #[test]
    fn inspecting_asks_for_the_detail_of_the_selected_process() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let selected = state.selected().expect("selected");

        let effects = reduce(&mut state, Action::InspectSelected);

        assert_eq!(
            effects.as_slice(),
            &[Effect::FetchProcessDetail(selected), Effect::RequestRedraw]
        );
        assert_eq!(state.input_mode(), InputMode::ProcessDetail);
        assert_eq!(state.detail_request(), Some(selected));
    }

    #[test]
    fn a_detail_reply_for_another_process_is_discarded() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let _ = reduce(&mut state, Action::InspectSelected);

        let stranger =
            ProcessDetail::pending(identity(404, 404), std::time::SystemTime::UNIX_EPOCH);
        let effects = apply::<()>(
            &mut state,
            Event::Detail(ProcessDetailResult::Loaded(Box::new(stranger))),
        );

        assert!(effects.is_empty());
        assert!(state.detail().is_none());
    }

    #[test]
    fn a_detail_reply_for_the_awaited_process_is_kept() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let selected = state.selected().expect("selected");
        let _ = reduce(&mut state, Action::InspectSelected);

        let detail = ProcessDetail::pending(selected, std::time::SystemTime::UNIX_EPOCH);
        let effects = apply::<()>(
            &mut state,
            Event::Detail(ProcessDetailResult::Loaded(Box::new(detail))),
        );

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.detail().map(|detail| detail.identity), Some(selected));
        assert_eq!(state.detail_request(), None);
    }

    #[test]
    fn a_vanished_detail_closes_the_overlay_and_reports_it() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let selected = state.selected().expect("selected");
        let _ = reduce(&mut state, Action::InspectSelected);

        let effects = apply::<()>(
            &mut state,
            Event::Detail(ProcessDetailResult::Vanished(selected)),
        );

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert!(
            !state
                .overlays()
                .iter()
                .any(|overlay| { overlay.kind() == OverlayKind::ProcessDetail })
        );
        assert_eq!(state.detail_request(), None);
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("exited")),
        );
    }

    #[test]
    fn a_reused_detail_reply_is_reported_as_reuse() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SelectFirst);
        let selected = state.selected().expect("selected");
        let _ = reduce(&mut state, Action::InspectSelected);

        let _ = apply::<()>(
            &mut state,
            Event::Detail(ProcessDetailResult::Reused {
                requested: selected,
                found: identity(selected.pid, 999_999),
            }),
        );

        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("different process")),
        );
        assert!(state.detail().is_none());
    }

    // -------------------------------------------------------- overlays and modes

    #[test]
    fn help_toggles_and_owns_the_keyboard_while_open() {
        let mut state = running();

        let effects = reduce(&mut state, Action::ToggleHelp);
        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.input_mode(), InputMode::Help);

        let _ = reduce(&mut state, Action::ToggleHelp);
        assert_eq!(state.input_mode(), InputMode::Normal);
    }

    #[test]
    fn the_sort_selector_reuses_the_list_bindings_and_commits_on_enter() {
        let mut state = running();
        let before = state.sort();

        let _ = reduce(&mut state, Action::OpenSortSelector);
        assert_eq!(
            state.input_mode(),
            InputMode::Normal,
            "§6.1 has no mode for the selector"
        );
        let _ = reduce(&mut state, Action::SelectNext);
        let effects = reduce(&mut state, Action::InspectSelected);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_ne!(state.sort().key, before.key);
        assert!(state.overlays().is_empty());
    }

    #[test]
    fn a_new_column_starts_in_its_natural_direction() {
        let mut state = running();
        let _ = reduce(&mut state, Action::SetSort(SortField::Name));
        assert_eq!(state.sort().key, ProcessSortKey::Name);
        assert_eq!(
            state.sort().direction,
            SortDirection::Ascending,
            "a text column reads as a list (§7.2)"
        );

        let _ = reduce(&mut state, Action::SetSort(SortField::Cpu));
        assert_eq!(state.sort().direction, SortDirection::Descending);

        assert!(
            reduce(&mut state, Action::SetSort(SortField::Cpu)).is_empty(),
            "re-selecting the same column is not a flip; `S` reverses"
        );
    }

    #[test]
    fn escape_leaves_the_time_lens_without_returning_to_live() {
        let mut state = running();
        deliver(&mut state, 2, &table());
        state.focus = PanelFocus::History;
        assert_eq!(state.input_mode(), InputMode::TimeLens);
        let _ = reduce(&mut state, Action::SeekHistory(Seek::step_back()));

        let effects = reduce(&mut state, Action::CancelOverlay);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.focus(), PanelFocus::Processes);
        assert_eq!(state.input_mode(), InputMode::Normal);
        assert!(
            state.timeline().is_historical(),
            "§2.1: only L returns to live"
        );
    }

    #[test]
    fn escape_with_nothing_open_and_the_table_focused_does_nothing() {
        let mut state = running();
        assert!(reduce(&mut state, Action::CancelOverlay).is_empty());
    }

    #[test]
    fn the_help_overlay_scrolls_within_its_content() {
        let mut state = running();
        let _ = reduce(&mut state, Action::ToggleHelp);

        let effects = reduce(&mut state, Action::SelectNext);
        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.top_overlay().and_then(Overlay::scroll), Some(1));

        let _ = reduce(&mut state, Action::SelectLast);
        let limit = help_line_count(&state.help()).saturating_sub(1);
        assert_eq!(state.top_overlay().and_then(Overlay::scroll), Some(limit));
        assert!(
            reduce(&mut state, Action::SelectNext).is_empty(),
            "scrolling stops at the last line"
        );

        let _ = reduce(&mut state, Action::SelectFirst);
        assert_eq!(state.top_overlay().and_then(Overlay::scroll), Some(0));
    }

    // ------------------------------------------------------- command palette

    #[test]
    fn the_palette_runs_a_typed_command() {
        let mut state = running();
        let _ = reduce(&mut state, Action::OpenCommandPalette);
        assert_eq!(state.input_mode(), InputMode::CommandPalette);
        for character in "view storage".chars() {
            let _ = reduce(&mut state, Action::InsertChar(character));
        }

        let effects = reduce(&mut state, Action::SubmitInput);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.view(), ViewId::Storage);
        assert!(state.overlays().is_empty());
    }

    #[test]
    fn the_palette_completes_a_half_typed_command_instead_of_scolding() {
        let mut state = running();
        let _ = reduce(&mut state, Action::OpenCommandPalette);
        for character in "sor".chars() {
            let _ = reduce(&mut state, Action::InsertChar(character));
        }

        let effects = reduce(&mut state, Action::SubmitInput);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(
            state
                .top_overlay()
                .and_then(Overlay::text_input)
                .map(TextInput::text),
            Some("sort "),
            "§6.3 exists for discoverability"
        );
    }

    #[test]
    fn an_unknown_command_is_reported_and_the_palette_stays_open() {
        let mut state = running();
        let _ = reduce(&mut state, Action::OpenCommandPalette);
        for character in "banana".chars() {
            let _ = reduce(&mut state, Action::InsertChar(character));
        }

        let effects = reduce(&mut state, Action::SubmitInput);

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.input_mode(), InputMode::CommandPalette);
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("banana")),
        );
    }

    #[test]
    fn the_palette_export_command_returns_an_effect_rather_than_writing() {
        let mut state = running();
        let _ = reduce(&mut state, Action::OpenCommandPalette);
        for character in "export snapshot /tmp/monitrs.json".chars() {
            let _ = reduce(&mut state, Action::InsertChar(character));
        }

        let effects = reduce(&mut state, Action::SubmitInput);

        assert_eq!(
            effects.as_slice(),
            &[
                Effect::ExportSnapshot(std::path::PathBuf::from("/tmp/monitrs.json")),
                Effect::RequestRedraw
            ]
        );
    }

    #[test]
    fn the_palette_reload_command_returns_an_effect() {
        let mut state = running();
        let _ = reduce(&mut state, Action::OpenCommandPalette);
        for character in "reload config".chars() {
            let _ = reduce(&mut state, Action::InsertChar(character));
        }

        let effects = reduce(&mut state, Action::SubmitInput);

        assert!(effects.contains(&Effect::ReloadConfig));
    }

    #[test]
    fn config_path_reports_where_configuration_came_from() {
        let mut state = AppState::new(AppSettings {
            started_at: epoch(),
            size: (160, 48),
            config_path: Some(std::path::PathBuf::from("/home/gabor/.config/monitrs.toml")),
            ..AppSettings::default()
        });

        let _ = run_command(&mut state, Command::ConfigPath);

        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("monitrs.toml")),
        );
    }

    #[test]
    fn the_history_command_rebuilds_the_ring_and_returns_to_live() {
        let mut state = running();
        deliver(&mut state, 2, &table());
        let _ = reduce(&mut state, Action::SeekHistory(Seek::step_back()));
        assert_eq!(state.history().len(), 2);

        let _ = run_command(&mut state, Command::History(Duration::from_secs(60)));

        assert_eq!(state.history().len(), 0, "a rebuilt ring starts empty");
        assert_eq!(state.timeline_status(), TimelineStatus::Live);
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("discarded")),
            "§8.5: the consequence is reported"
        );
        assert_eq!(
            state.history().limits().effective_duration(),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn the_interval_command_clamps_and_reports() {
        let mut state = running();

        let _ = run_command(&mut state, Command::Interval(Duration::from_millis(1)));

        assert_eq!(state.sample_interval(), MIN_SAMPLE_INTERVAL);
        assert!(
            state
                .notices()
                .iter()
                .any(|notice| notice.message.contains("outside the supported range")),
        );
    }

    #[test]
    fn palette_display_commands_change_only_presentation() {
        let mut state = running();

        let _ = run_command(&mut state, Command::Theme(ThemeId::HighContrast));
        let _ = run_command(&mut state, Command::Glyphs(crate::glyphs::GlyphMode::Ascii));
        let _ = run_command(&mut state, Command::Color(crate::theme::ColorMode::Off));

        assert_eq!(state.display().theme, ThemeId::HighContrast);
        assert!(state.glyph_set().is_ascii());
        assert_eq!(state.color_depth(), crate::theme::ColorDepth::Off);
        assert!(state.display().color_explicit);
    }

    // ------------------------------------------------------------------ search

    #[test]
    fn n_and_shift_n_move_between_text_matches() {
        let mut state = state();
        let processes = vec![
            Fake::new(1, 1, "launchd").cpu(0.1),
            Fake::new(2, 2, "rustc").cpu(90.0),
            Fake::new(3, 3, "zsh").cpu(5.0),
            Fake::new(4, 4, "rustc").cpu(80.0),
        ];
        deliver(&mut state, 1, &processes);
        // The pattern both narrows the table and defines what `n` searches for; two
        // rows survive it, so there is somewhere to move between.
        let _ = reduce(&mut state, Action::SetFilter("rustc".to_owned()));
        let _ = reduce(&mut state, Action::SelectFirst);

        let effects = reduce(&mut state, Action::NextMatch);
        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.selected(), Some(identity(4, 4)));

        let _ = reduce(&mut state, Action::PreviousMatch);
        assert_eq!(state.selected(), Some(identity(2, 2)));
    }

    #[test]
    fn searching_without_a_pattern_does_nothing() {
        let mut state = running();
        assert!(reduce(&mut state, Action::NextMatch).is_empty());
        assert!(reduce(&mut state, Action::PreviousMatch).is_empty());
    }

    // ------------------------------------------------------------------- rows

    #[test]
    fn a_snapshot_with_no_processes_is_a_state_not_an_error() {
        let mut state = state();
        deliver(&mut state, 1, &[]);

        assert!(state.rows().is_empty());
        assert_eq!(state.selected(), None);
        assert!(reduce(&mut state, Action::SelectNext).is_empty());
        assert!(reduce(&mut state, Action::InspectSelected).is_empty());
        assert!(state.notices().is_empty());
    }

    #[test]
    fn tree_mode_keeps_children_under_their_parents() {
        let mut state = state();
        let processes = vec![
            Fake::new(1, 1, "launchd").cpu(0.1),
            Fake::new(2, 2, "zsh").parent(1).cpu(1.0),
            Fake::new(3, 3, "cargo").parent(2).cpu(200.0),
        ];
        deliver(&mut state, 1, &processes);

        let _ = reduce(&mut state, Action::ToggleTreeView);

        assert!(state.is_tree_view());
        let depths: Vec<u32> = state
            .rows()
            .as_slice()
            .iter()
            .filter_map(|row| row.tree.map(|shape| shape.depth))
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn the_history_ring_records_every_snapshot_the_reducer_accepts() {
        let mut state = state();
        for sequence in 1..=5 {
            deliver(&mut state, sequence, &table());
        }
        assert_eq!(state.history().len(), 5);
        assert_eq!(
            state.history().newest().map(|sample| sample.sequence),
            Some(5)
        );
    }

    #[test]
    fn the_first_snapshot_selects_the_first_row_and_asks_for_a_redraw() {
        let mut state = state();
        let effects = apply::<()>(&mut state, Event::Snapshot(arc_snapshot(1, &table())));

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert_eq!(state.selected_row(), Some(0));
        assert_eq!(state.rows().len(), 5);
    }

    #[test]
    fn a_warming_up_snapshot_produces_no_measurements_and_still_renders_rows() {
        let mut state = state();
        let warming = Arc::new(SystemSnapshot::warming_up(
            epoch(),
            std::time::SystemTime::UNIX_EPOCH,
            8,
        ));
        let effects = apply::<()>(&mut state, Event::Snapshot(warming));

        assert_eq!(effects.as_slice(), &[Effect::RequestRedraw]);
        assert!(state.rows().is_empty());
        assert_eq!(state.selection().identity(), None);
    }

    #[test]
    fn resyncing_reports_what_happened_to_the_selection() {
        let mut state = state();
        deliver(&mut state, 1, &table());
        // Nothing has been chosen yet, so the cursor re-derives from row 0.
        assert_eq!(state.resync_rows(), Resync::Initialised { row: 0 });

        // Once a row is the user's, resyncing keeps that process.
        let _ = reduce(&mut state, Action::SelectNext);
        assert_eq!(state.resync_rows(), Resync::Retained { row: 1 });
    }

    #[test]
    fn the_detail_line_count_grows_with_the_ancestry_children_and_descriptors() {
        let bare = ProcessDetail::pending(identity(1, 1), std::time::SystemTime::UNIX_EPOCH);
        let bare_lines = detail_line_count(Some(&bare));
        assert!(bare_lines >= 10);
        assert_eq!(detail_line_count(None), 1);

        let mut populated = bare.clone();
        populated.children = MetricState::Available(vec![identity(2, 2), identity(3, 3)]);
        assert_eq!(detail_line_count(Some(&populated)), bare_lines + 2);

        // One row per *listed* descriptor, not per descriptor the process holds: the
        // ones the cap left out have no row to scroll to.
        let mut with_files = bare.clone();
        with_files.open_file_list =
            MetricState::Available(monitrs_core::model::OpenFileList::listed(
                vec![monitrs_core::model::OpenFileEntry {
                    descriptor: 0,
                    kind: monitrs_core::model::OpenFileKind::File,
                    path: MetricState::Available("/dev/null".into()),
                }],
                4_096,
            ));
        assert_eq!(detail_line_count(Some(&with_files)), bare_lines + 1);
    }
}
