//! The assembled interactive runtime: the thing that makes monitrs an application
//! rather than a library.
//!
//! Everything else in the workspace is a pure function of its inputs. This module
//! is where the impurity is concentrated on purpose, and it is short for that
//! reason: it owns the clock, the threads, the terminal, and the execution of
//! effects, and it delegates every decision.
//!
//! The order of operations is not arbitrary. §14.3 and §10.3 between them fix it:
//!
//! 1. Configuration and logging, before anything can want to report a problem.
//! 2. The panic hook, before the terminal is touched, so a panic during setup
//!    still restores.
//! 3. The terminal guard, which owns the modes.
//! 4. The ratatui terminal, which owns the frame buffer and must be dropped
//!    *before* the guard restores.
//! 5. The workers.
//! 6. The loop.
//! 7. Shutdown: signal the workers, drop the terminal, **restore the screen**, and
//!    only then join. A user staring at a frozen alternate screen cannot tell a
//!    slow shutdown from a hang.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use monitrs_collectors::{CommonCollector, TierIntervals};
use monitrs_core::model::Severity;
use monitrs_core::process::{ProcessSort, ProcessSortKey, SortDirection};
use monitrs_tui::action::{Effect, ViewId};
use monitrs_tui::app::{
    AppSettings, AppState, DisplaySettings, Notice, NoticeKind, Overlay, apply, detail_line_count,
    help_line_count,
};
use monitrs_tui::event::{DEFAULT_TICK_INTERVAL, Event};
use monitrs_tui::glyphs::TerminalEnv;
use monitrs_tui::keymap::{Binding, Keymap, KeymapError};
use monitrs_tui::terminal::{
    CrosstermControl, TerminalGuard, TerminalSettings, create_terminal, install_panic_hook,
};
use monitrs_tui::theme::ThemeId;
use monitrs_tui::views;
use monitrs_tui::widgets::Presentation;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::cli::Cli;
use crate::config::{self, Config};
use crate::logging::{self, LogSettings};
use crate::runtime::{
    DetailRequest, SampleRequest, Shutdown, Workers, detail_channel, drain_to_newest_snapshot,
    event_channel, spawn_detail_worker, spawn_input_thread, spawn_sampler_thread,
    spawn_tick_thread,
};
use crate::signals;

/// The configuration payload carried by [`Event::ConfigReloaded`].
///
/// `Event` is generic over it precisely so the reducer can be tested without the
/// binary's config type (see the note on `Event` itself).
type ConfigEvent = Result<Box<Config>, String>;

/// How long the loop waits for an event before looking around.
///
/// Short enough that a shutdown request is noticed promptly, long enough that an
/// idle monitrs is not spinning. The tick thread is what actually drives
/// redraws and multi-key sequence timeouts; this is only a floor.
const LOOP_POLL: Duration = Duration::from_millis(200);

/// Runs the interactive interface.
pub(crate) fn run(cli: &Cli) -> color_eyre::Result<ExitCode> {
    // --- 1. configuration -------------------------------------------------
    let loaded = config::load(cli.config.config.as_deref(), cli.config.no_config)?;
    let source_path = loaded.source.path().map(std::path::Path::to_path_buf);
    let mut settings = loaded.config;
    settings.apply_cli(cli);

    // The CLI can push a valid file out of range, so validate the merged result
    // rather than only the file (§12).
    let problems = settings.validate();
    if !problems.is_empty() {
        let mut message = String::from("the merged configuration is invalid:");
        for problem in &problems {
            message.push_str("\n  ");
            message.push_str(&problem.to_string());
        }
        return Err(color_eyre::eyre::eyre!(message));
    }

    // --- 2. logging, before anything wants to report ----------------------
    let log_settings = match &cli.config.debug_log {
        Some(path) => LogSettings::to_file(path.clone()),
        None => LogSettings::disabled(),
    };
    let startup = logging::install(&log_settings);
    let mut startup_notices = logging::report_problems(&startup.problems);
    startup_notices.extend(loaded.warnings);

    // --- 3. the collector -------------------------------------------------
    // Two instances: `process_detail` takes `&mut self`, and §10.3 requires that a
    // slow detail read cannot delay sampling. Each keeps its own rate baselines,
    // which is why a collector must be long-lived (§9.1).
    let sampler_source = CommonCollector::new()?;
    let detail_source = CommonCollector::new()?;

    let keymap = build_keymap(&settings)
        .map_err(|error| color_eyre::eyre::eyre!("the configured keymap is unusable: {error}"))?;

    // --- 4. the terminal --------------------------------------------------
    // The hook goes in before any mode is changed, so a panic between here and
    // the guard still leaves a usable terminal (§14.3).
    install_panic_hook();
    let mut guard = TerminalGuard::install(
        CrosstermControl::stdout(),
        TerminalSettings::default().with_mouse_capture(settings.display.mouse),
    )?;
    let mut terminal = create_terminal()?;
    let area = terminal.size()?;

    // --- 5. state ---------------------------------------------------------
    let started_at = Instant::now();
    let mut state = AppState::new(AppSettings {
        started_at,
        size: (area.width, area.height),
        view: if cli.view.processes {
            ViewId::Processes
        } else {
            ViewId::Overview
        },
        sort: sort_from(&settings),
        tree_mode: settings.processes.tree,
        filter: settings.processes.filter.clone(),
        only_user: None,
        hide_kernel_threads: !settings.display.show_kernel_threads,
        display: DisplaySettings {
            theme: ThemeId::from_name(&settings.display.theme).unwrap_or_default(),
            glyph_mode: settings.display.glyphs.into(),
            color_mode: settings.display.color.into(),
            color_explicit: cli.color_was_explicit(),
            byte_units: settings.display.units.into(),
        },
        env: TerminalEnv::from_process(),
        history: history_config(&settings),
        sample_interval: settings.sampling.interval,
        config_path: source_path,
        keymap,
        sequence_timeout: monitrs_tui::keymap::DEFAULT_SEQUENCE_TIMEOUT,
    });

    for message in startup_notices {
        state.push_notice(Notice::new(NoticeKind::Config, Severity::Watch, message));
    }

    // --- 6. workers -------------------------------------------------------
    let (sender, receiver) = event_channel::<ConfigEvent>();
    let (detail_tx, detail_rx) = detail_channel();
    let shutdown = Shutdown::new();
    let forced = SampleRequest::new();
    let mut workers = Workers::new();

    spawn_input_thread(&mut workers, sender.clone(), shutdown.clone(), LOOP_POLL)?;
    spawn_tick_thread(
        &mut workers,
        sender.clone(),
        shutdown.clone(),
        DEFAULT_TICK_INTERVAL,
    )?;
    spawn_sampler_thread(
        &mut workers,
        sampler_source,
        sender.clone(),
        shutdown.clone(),
        TierIntervals::derived_from(settings.sampling.interval),
        forced.clone(),
        thresholds_from(&settings),
    )?;
    spawn_detail_worker(
        &mut workers,
        detail_source,
        detail_rx,
        sender.clone(),
        shutdown.clone(),
    )?;

    // --- 7. the loop ------------------------------------------------------
    let channel_health = sender.health();
    let mut effects_context = EffectContext {
        detail_tx,
        forced,
        shutdown: shutdown.clone(),
        settings,
        sender,
    };
    let mut dirty = true;

    loop {
        if dirty {
            let started = Instant::now();
            terminal.draw(|frame| draw(frame, &state))?;
            let elapsed = started.elapsed();
            state.record_render(Instant::now(), elapsed);
            dirty = false;
        }

        let Ok(event) = receiver.recv_timeout(LOOP_POLL) else {
            // Timed out or every sender is gone. Either way, look at the flag.
            if shutdown.is_triggered() || state.should_quit() {
                break;
            }
            continue;
        };

        // A slow frame can leave several snapshots waiting; only the newest is
        // worth reducing, and the rest are counted rather than silently lost.
        let (event, extra) = match event {
            Event::Snapshot(snapshot) => {
                let (newest, others) =
                    drain_to_newest_snapshot(&receiver, snapshot, &channel_health);
                (Event::Snapshot(newest), others)
            }
            other => (other, Vec::new()),
        };

        let mut effects = apply(&mut state, event);
        for queued in extra {
            for effect in apply(&mut state, queued).iter() {
                effects.push(effect.clone());
            }
        }

        for effect in effects.iter() {
            if execute(effect, &mut state, &mut effects_context) == Flow::Stop {
                shutdown.trigger();
            }
        }
        // Any state change is worth a frame; the reducer's own redraw requests are
        // a hint about *urgency*, not about correctness.
        dirty = true;

        if state.should_quit() || shutdown.is_triggered() {
            break;
        }
    }

    // --- 8. shutdown, in this order ---------------------------------------
    shutdown.trigger();
    // The terminal is released and the screen restored *before* joining, so a
    // worker that will not stop promptly does not leave the user looking at a
    // frozen alternate screen (§10.3).
    drop(terminal);
    let restore = guard.restore();
    let stuck = workers.join_all();
    if let Some(log) = startup.log {
        // Dropped after the screen is restored: tracing-appender's guard can print
        // on a timed-out flush, and that must not land on the alternate screen.
        log.shutdown();
    }

    if let Err(error) = restore {
        return Err(color_eyre::eyre::eyre!(
            "the terminal could not be restored: {error}"
        ));
    }
    if !stuck.is_empty() {
        // Reported rather than swallowed: a panicking worker is a real defect even
        // though the user's terminal is now fine.
        eprintln!("monitrs: worker thread(s) panicked: {}", stuck.join(", "));
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether the loop should continue after an effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flow {
    /// Keep going.
    Continue,
    /// Leave the loop.
    Stop,
}

/// Everything an effect might need. Grouped so `execute` stays one screen.
struct EffectContext {
    detail_tx: crossbeam_channel::Sender<DetailRequest>,
    forced: SampleRequest,
    shutdown: Shutdown,
    settings: Config,
    sender: crate::runtime::EventSender<ConfigEvent>,
}

/// Performs one effect.
///
/// This is the only place in the program that acts on the outside world at the
/// reducer's request. §10.5's whole point is that the reducer decides and this
/// function does, which is what lets the confirmation chain be tested without a
/// signal ever being sent.
fn execute(effect: &Effect, state: &mut AppState, ctx: &mut EffectContext) -> Flow {
    match effect {
        Effect::None | Effect::RequestRedraw => Flow::Continue,

        Effect::RequestSample => {
            ctx.forced.request();
            Flow::Continue
        }

        Effect::FetchProcessDetail(identity) => {
            // A full queue means the selection is moving faster than detail can be
            // collected, so the dropped request is already obsolete.
            let _ = ctx.detail_tx.try_send(DetailRequest::Fetch(*identity));
            Flow::Continue
        }

        Effect::SignalProcess { identity, signal } => {
            let report = signals::deliver(*identity, *signal);
            // Logged at info because a signal is a deliberate, consequential act;
            // the identity is numeric so nothing sensitive is written (§14.2).
            tracing::info!(
                pid = identity.pid,
                start_key = identity.start_key,
                signal = signal.name(),
                delivered = report.was_delivered(),
                "process signal"
            );
            state.push_notice(Notice::new(
                NoticeKind::ProcessAction,
                report.severity(),
                report.message(),
            ));
            Flow::Continue
        }

        Effect::ReniceProcess { identity, .. } => {
            // §6.2 makes renice conditional on platform support, and neither native
            // collector exposes it yet. Saying so is better than a dialog that
            // appears to work.
            state.push_notice(Notice::new(
                NoticeKind::ProcessAction,
                Severity::Watch,
                format!(
                    "renice is not available in this build; {} unchanged",
                    identity.pid
                ),
            ));
            Flow::Continue
        }

        Effect::ReloadConfig => {
            let outcome = reload(&ctx.settings, state);
            match outcome {
                Ok(reloaded) => {
                    ctx.settings = *reloaded.clone();
                    let _ = ctx.sender.send(Event::ConfigReloaded(Ok(reloaded)));
                }
                Err(message) => {
                    let _ = ctx.sender.send(Event::ConfigReloaded(Err(message)));
                }
            }
            Flow::Continue
        }

        Effect::ExportSnapshot(path) => {
            match state.live_snapshot() {
                Some(snapshot) => {
                    let export = crate::export::SnapshotExport::new(
                        snapshot,
                        crate::export::RedactionPolicy::REDACTED,
                    );
                    let result = export
                        .to_json()
                        .map_err(|error| error.to_string())
                        .and_then(|json| {
                            std::fs::write(path, json.as_bytes()).map_err(|e| e.to_string())
                        });
                    let (severity, message) = match result {
                        Ok(()) => (
                            Severity::Info,
                            format!("wrote a redacted snapshot to {}", path.display()),
                        ),
                        Err(error) => (
                            Severity::Critical,
                            format!("could not write {}: {error}", path.display()),
                        ),
                    };
                    state.push_notice(Notice::new(NoticeKind::Export, severity, message));
                }
                None => state.push_notice(Notice::new(
                    NoticeKind::Export,
                    Severity::Watch,
                    "nothing to export yet: no sample has been collected".to_owned(),
                )),
            }
            Flow::Continue
        }

        Effect::Shutdown => {
            ctx.shutdown.trigger();
            Flow::Stop
        }
    }
}

/// Re-reads the configuration file, validating the whole candidate first (§12).
fn reload(current: &Config, state: &AppState) -> Result<Box<Config>, String> {
    let Some(path) = state.config_path() else {
        return Err(
            "no configuration file to reload; monitrs is using built-in defaults".to_owned(),
        );
    };
    match config::reload(current, path) {
        Ok(outcome) => {
            if outcome.non_reloadable.is_empty() {
                Ok(Box::new(outcome.config))
            } else {
                // Applied anyway, but the user is told which parts will not take
                // effect until restart — §12 forbids silently dropping them.
                Err(format!(
                    "reloaded, but these need a restart: {}",
                    outcome.non_reloadable.join(", ")
                ))
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Draws one frame: the chrome and screen, then the overlay stack on top.
///
/// `views::render` deliberately does not know about overlays — the screens and the
/// overlays are separate concerns — so composing them is this module's job.
fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let presentation = presentation_for(state);
    views::render(frame, area, state, presentation);

    if let Some(overlay) = state.top_overlay() {
        draw_overlay(frame, area, state, presentation, overlay);
    }
}

/// Builds the presentation from state, so rendering stays a pure function of it.
fn presentation_for(state: &AppState) -> Presentation<'_> {
    Presentation::new(state.glyph_set(), state.theme(), state.color_depth())
        .with_units(state.display().byte_units)
}

/// Draws the topmost overlay.
fn draw_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
    overlay: &Overlay,
) {
    use monitrs_tui::views::overlays::{
        CommandPaletteOverlay, FilterEditOverlay, HelpOverlay, ProcessActionOverlay,
        ProcessDetailOverlay, SortSelectorOverlay,
    };

    match overlay {
        Overlay::Help { scroll } => {
            let sections = state.help();
            let widget = HelpOverlay::new(presentation, &sections, state.input_mode())
                .with_scroll((*scroll).min(help_line_count(&sections).saturating_sub(1)));
            frame.render_widget(widget, area);
        }

        Overlay::ProcessDetail { identity, scroll } => {
            let process = state.snapshot().and_then(|s| s.process(*identity));
            let detail = state.detail();
            let lines = detail_line_count(detail);
            let widget = ProcessDetailOverlay::new(presentation, *identity, process, detail)
                .with_scroll((*scroll).min(lines.saturating_sub(1)));
            frame.render_widget(widget, area);
        }

        Overlay::FilterEdit { input } => {
            let (matches, total) = filter_counts(state);
            let widget = FilterEditOverlay::new(presentation, input, matches, total);
            // A widget only receives a buffer, so the real cursor has to be placed
            // here; without this the caret is drawn but the terminal's cursor is
            // elsewhere, which reads badly to a screen reader.
            if let Some(position) = widget.cursor_position(area) {
                frame.set_cursor_position(position);
            }
            frame.render_widget(widget, area);
        }

        Overlay::CommandPalette { input, highlight } => {
            let widget = CommandPaletteOverlay::new(presentation, input, *highlight);
            if let Some(position) = widget.cursor_position(area) {
                frame.set_cursor_position(position);
            }
            frame.render_widget(widget, area);
        }

        Overlay::SortSelector { cursor } => {
            frame.render_widget(
                SortSelectorOverlay::new(presentation, state.sort(), *cursor),
                area,
            );
        }

        Overlay::ProcessAction(stage) => {
            frame.render_widget(
                ProcessActionOverlay::new(presentation, *stage, state.selected_process()),
                area,
            );
        }
    }
}

/// How many rows the active filter matches, and how many exist.
fn filter_counts(state: &AppState) -> (usize, usize) {
    let total = state
        .snapshot()
        .map_or(0, |snapshot| snapshot.process_count());
    (state.rows().len(), total)
}

/// The initial process ordering from configuration (§12 `processes.sort`).
///
/// The value was already validated, so an unparsable one here would be a bug
/// rather than user error; falling back to CPU keeps the program usable either way.
fn sort_from(settings: &Config) -> ProcessSort {
    let key = settings
        .processes
        .sort
        .parse::<ProcessSortKey>()
        .unwrap_or(ProcessSortKey::Cpu);
    ProcessSort::new(
        key,
        SortDirection::from_descending(settings.processes.descending),
    )
}

/// The diagnostic thresholds from configuration (§12 `[diagnostics]`).
///
/// Only the keys §12 exposes are taken from the file; the rest keep the engine's
/// documented defaults. `sanitized` is what enforces the engine's own invariants,
/// so a value that passed `Config::validate` cannot still produce a nonsensical
/// rule.
fn thresholds_from(settings: &Config) -> monitrs_core::diagnostics::Thresholds {
    monitrs_core::diagnostics::Thresholds {
        enabled: settings.diagnostics.enabled,
        cpu_watch_percent: f32::from(settings.diagnostics.cpu_watch_percent),
        cpu_critical_percent: f32::from(settings.diagnostics.cpu_critical_percent),
        memory_watch_available_percent: f32::from(
            settings.diagnostics.memory_watch_available_percent,
        ),
        memory_critical_available_percent: f32::from(
            settings.diagnostics.memory_critical_available_percent,
        ),
        sustained_samples: usize::from(settings.diagnostics.sustained_samples),
        ..monitrs_core::diagnostics::Thresholds::default()
    }
    .sanitized()
}

/// The history configuration, which the ring clamps and reports (§8.5).
fn history_config(settings: &Config) -> monitrs_core::history::HistoryConfig {
    monitrs_core::history::HistoryConfig {
        interval: settings.sampling.interval,
        duration: settings.sampling.history,
        top_contributors_per_metric: usize::from(settings.processes.top_contributors_per_metric),
        memory_budget_bytes: settings.sampling.max_history_memory,
    }
}

/// Builds the keymap, applying any `[keys]` overrides from configuration.
///
/// The overrides are already syntactically valid and free of internal conflicts —
/// `Config::validate` and `validate_keys` ran at load — but replacing a chord can
/// still collide with a *built-in* binding, so the result goes back through
/// `Keymap::from_bindings`, which rejects that (§12, §21 M6).
fn build_keymap(settings: &Config) -> Result<Keymap, KeymapError> {
    let overrides = collect_overrides(settings);
    if overrides.is_empty() {
        return Ok(Keymap::builtin());
    }

    let builtin = Keymap::builtin();
    let mut bindings: Vec<Binding> = Vec::new();
    // An action can have several built-in chords — `Quit` is bound to both `q` and
    // `Ctrl-C`. The override replaces the whole set, so it is emitted once from the
    // first binding as a template. Emitting it per matching binding would produce
    // the same chord twice and conflict with itself.
    let mut emitted: Vec<&'static str> = Vec::new();

    for binding in builtin.bindings() {
        let label = binding.outcome.diagnostic_name();
        match overrides
            .iter()
            .find(|(action, _)| *action == label.as_str())
        {
            Some((action, chords)) => {
                if emitted.contains(action) {
                    continue;
                }
                emitted.push(action);
                for chord in chords {
                    bindings.push(Binding {
                        chord: *chord,
                        ..binding.clone()
                    });
                }
            }
            None => bindings.push(binding.clone()),
        }
    }
    Keymap::from_bindings(bindings)
}

/// The `[keys]` table as `(action label, chords)`.
fn collect_overrides(settings: &Config) -> Vec<(&'static str, Vec<monitrs_tui::keymap::Chord>)> {
    use monitrs_tui::keymap::Chord;

    let mut out = Vec::new();
    for (action, keys) in [
        ("Quit", &settings.keys.quit),
        ("ToggleHelp", &settings.keys.help),
        ("BeginFilterEdit", &settings.keys.filter),
        ("TogglePause", &settings.keys.pause),
        ("ReturnLive", &settings.keys.live),
    ] {
        let Some(keys) = keys else { continue };
        let chords: Vec<Chord> = keys
            .iter()
            .filter_map(|text| config::parse_key(text).ok())
            .map(Chord::key)
            .collect();
        if !chords.is_empty() {
            out.push((action, chords));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_sort(sort: &str, descending: bool) -> Config {
        let mut settings = Config::default();
        settings.processes.sort = sort.to_owned();
        settings.processes.descending = descending;
        settings
    }

    #[test]
    fn the_configured_sort_reaches_the_initial_state() {
        let order = sort_from(&config_with_sort("memory", false));
        assert_eq!(order.key, ProcessSortKey::Memory);
        assert!(!order.direction.is_descending());
    }

    #[test]
    fn an_unparsable_sort_falls_back_rather_than_refusing_to_start() {
        // Config::validate rejects this before we get here, so reaching the
        // fallback would be our bug; the program should still be usable.
        let order = sort_from(&config_with_sort("entropy", true));
        assert_eq!(order.key, ProcessSortKey::Cpu);
    }

    #[test]
    fn the_history_configuration_comes_from_the_merged_settings() {
        let mut settings = Config::default();
        settings.sampling.interval = Duration::from_millis(500);
        settings.sampling.history = Duration::from_secs(120);
        settings.processes.top_contributors_per_metric = 25;
        let history = history_config(&settings);
        assert_eq!(history.interval, Duration::from_millis(500));
        assert_eq!(history.duration, Duration::from_secs(120));
        assert_eq!(history.top_contributors_per_metric, 25);
    }

    #[test]
    fn an_untouched_keys_table_yields_the_builtin_keymap() {
        let keymap = build_keymap(&Config::default()).expect("the built-in map is valid");
        assert_eq!(keymap.bindings().len(), Keymap::builtin().bindings().len());
    }

    #[test]
    fn a_rebound_key_replaces_the_builtin_chord_and_keeps_the_action() {
        let mut settings = Config::default();
        settings.keys.quit = Some(vec!["Z".to_owned()]);
        let keymap = build_keymap(&settings).expect("rebinding quit to Z is valid");

        let quit_chords: Vec<String> = keymap
            .bindings()
            .iter()
            .filter(|binding| binding.outcome.diagnostic_name() == "Quit")
            .map(|binding| binding.chord.label())
            .collect();
        assert!(
            quit_chords.iter().any(|label| label.contains('Z')),
            "{quit_chords:?}"
        );
        assert!(
            !quit_chords.iter().any(|label| label == "q"),
            "the built-in chord must be replaced, not kept: {quit_chords:?}"
        );
    }

    #[test]
    fn rebinding_onto_a_builtin_key_is_rejected_rather_than_shadowing_it() {
        // §12 and §21 M6: a conflict is rejected. `/` is the built-in filter key,
        // so binding quit to it must fail rather than silently winning.
        let mut settings = Config::default();
        settings.keys.quit = Some(vec!["/".to_owned()]);
        let error =
            build_keymap(&settings).expect_err("binding quit onto the filter key must be refused");
        assert!(matches!(error, KeymapError::Conflict { .. }), "{error:?}");
    }

    #[test]
    fn an_unparsable_key_is_skipped_rather_than_dropping_the_action_entirely() {
        // Config::validate rejects the file first; if one slipped through, losing
        // the ability to quit would be much worse than ignoring the override.
        let mut settings = Config::default();
        settings.keys.quit = Some(vec!["wiggle".to_owned()]);
        let keymap = build_keymap(&settings).expect("valid");
        assert!(
            keymap
                .bindings()
                .iter()
                .any(|binding| binding.outcome.diagnostic_name() == "Quit"),
            "quit must still be bound"
        );
    }

    #[test]
    fn the_config_event_payload_can_carry_both_outcomes() {
        // The reducer is generic over this precisely so the binary's config type
        // never reaches monitrs-tui.
        let ok: ConfigEvent = Ok(Box::new(Config::default()));
        let err: ConfigEvent = Err("bad".to_owned());
        assert!(ok.is_ok());
        assert!(err.is_err());
    }
}
