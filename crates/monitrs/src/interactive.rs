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
//! 1. Configuration, before anything can want to report a problem. Logging is
//!    already running: `main` installs it for every subcommand and closes it after
//!    this function returns, which is how the log guard still outlives the restored
//!    terminal (§14.2). Problems it hit that could not be printed arrive here as
//!    `log_notices`.
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

use monitrs_collectors::{platform_collector, renice};
use monitrs_core::model::{MetricState, Severity};
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
use crate::runtime::{
    DetailRequest, SampleRequest, SamplingControl, Shutdown, Workers, detail_channel,
    drain_to_newest_snapshot, event_channel, spawn_detail_worker, spawn_input_thread,
    spawn_sampler_thread, spawn_signal_thread, spawn_tick_thread,
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
///
/// `log_notices` are the logging problems `main` could not print because it had
/// nowhere better to put them; they become in-UI notices below. The debug log
/// itself is owned by `main`, which closes it *after* this function has restored
/// the terminal (§14.2).
pub(crate) fn run(cli: &Cli, log_notices: Vec<String>) -> color_eyre::Result<ExitCode> {
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

    // --- 2. what could not be reported before there was a UI ---------------
    // Logging problems first, then configuration warnings, which is the order they
    // happened in.
    let mut startup_notices = log_notices;
    startup_notices.extend(loaded.warnings);

    // --- 3. the collector -------------------------------------------------
    // Two instances: `process_detail` takes `&mut self`, and §10.3 requires that a
    // slow detail read cannot delay sampling. Each keeps its own rate baselines,
    // which is why a collector must be long-lived (§9.1).
    //
    // `platform_collector` rather than `CommonCollector`: §9.2 enriches the
    // baseline natively by default, and naming the baseline here is exactly how a
    // build ends up quietly reporting a refused read as `0`.
    let sampler_source = platform_collector()?;
    let detail_source = platform_collector()?;

    let app_settings = settings_to_app(&settings, cli.color_was_explicit())
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
        env: TerminalEnv::from_process(),
        config_path: source_path,
        ..app_settings
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
    // Installed before the loop, so a `kill` at any point after this restores the
    // terminal instead of leaving it in raw mode (§14.3).
    let signals = spawn_signal_thread(&mut workers, shutdown.clone())?;
    spawn_tick_thread(
        &mut workers,
        sender.clone(),
        shutdown.clone(),
        DEFAULT_TICK_INTERVAL,
    )?;
    // The one piece of worker configuration that is not fixed at spawn: §6.3's
    // `interval` command and §12's reload both change it while the sampler runs.
    let sampling = SamplingControl::new(settings.sampling.interval, thresholds_from(&settings));
    spawn_sampler_thread(
        &mut workers,
        sampler_source,
        sender.clone(),
        shutdown.clone(),
        sampling.clone(),
        forced.clone(),
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
        mouse_at_startup: settings.display.mouse,
        color_explicit: cli.color_was_explicit(),
        settings,
        sender,
        sampling: sampling.clone(),
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
        // The reducer owns the sample interval — it is the thing that clamps a typed
        // value into range (§6.3) — so the sampler is told about it here rather than
        // through an effect. Comparing instead of storing unconditionally keeps this
        // to a single atomic load on the overwhelmingly common path where nothing
        // changed.
        if sampling.interval() != state.sample_interval() {
            sampling.set_interval(state.sample_interval());
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
    // Closed before the join, or the signal thread stays blocked on a signal that will
    // never arrive and `join_all` waits out its whole timeout.
    signals.close();
    // The terminal is released and the screen restored *before* joining, so a
    // worker that will not stop promptly does not leave the user looking at a
    // frozen alternate screen (§10.3).
    drop(terminal);
    let restore = guard.restore();
    let stuck = workers.join_all();
    // The debug log is *not* closed here. `main` owns it and drops it after this
    // function returns, which keeps the guarantee that mattered: tracing-appender's
    // guard can print on a timed-out flush, and by then the screen is the user's
    // again (§14.2).

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
    /// The sampler's live settings, so a reload reaches the thread and not only the
    /// screen.
    sampling: SamplingControl,
    /// Whether the terminal's mouse capture was on when the session started.
    ///
    /// Kept because `display.mouse` is the one setting a reload genuinely cannot
    /// apply — it is a terminal mode the guard set once (§14.3) — and saying so
    /// needs the old value to compare against.
    mouse_at_startup: bool,
    /// Whether `--color` was given on the command line (§5.2).
    ///
    /// Carried so a reload cannot quietly demote an explicit command-line choice to
    /// whatever the file and the environment happen to say.
    color_explicit: bool,
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

        Effect::ReniceProcess { identity, nice } => {
            // The target is rebuilt from the *live* snapshot rather than from
            // whatever the dialog was opened against, and `renice` rechecks the
            // identity again immediately before the write. Two rechecks rather than
            // one because the confirmation the user gave is about a process, not a
            // PID: §15.1's whole point is that a PID reused between the dialog and
            // the write must produce a refusal, not a renice of the wrong process.
            let target = state
                .live_snapshot()
                .and_then(|snapshot| {
                    snapshot
                        .processes
                        .iter()
                        .find(|process| process.identity == *identity)
                })
                .map(|process| {
                    renice::ReniceTarget::from_snapshot(
                        process,
                        state
                            .detail()
                            .filter(|detail| detail.identity == *identity)
                            .map_or(&MetricState::WarmingUp, |detail| &detail.nice),
                    )
                });
            let outcome = match target {
                Some(target) => renice::renice(&target, i32::from(*nice)),
                // Not in the newest snapshot: it exited between the confirmation and
                // now, which §14.1 calls expected. Reported as such rather than
                // attempted against a PID whose owner is unknown.
                None => renice::ReniceOutcome::Vanished,
            };
            // Info, like a signal: changing a process's priority is a deliberate,
            // consequential act, and the identity is numeric so nothing sensitive is
            // written (§14.2).
            tracing::info!(
                pid = identity.pid,
                start_key = identity.start_key,
                requested = *nice,
                applied = outcome.is_applied(),
                "process renice"
            );
            state.push_notice(Notice::new(
                NoticeKind::ProcessAction,
                if outcome.is_expected() {
                    Severity::Info
                } else {
                    Severity::Watch
                },
                outcome.message(),
            ));
            Flow::Continue
        }

        Effect::ReloadConfig => {
            match reload(&ctx.settings, state) {
                Ok(reloaded) => {
                    // Applied in three places, because the settings live in three:
                    // the runtime's own copy, the reducer's state, and the sampler
                    // thread. Missing any one of them is what made `:reload` report
                    // success and change nothing.
                    // Translated *before* anything is adopted, so an unusable keymap
                    // in the candidate leaves the running configuration alone (§12).
                    let translated = match settings_to_app(&reloaded.config, ctx.color_explicit) {
                        Ok(translated) => translated,
                        Err(error) => {
                            state.push_notice(Notice::new(
                                NoticeKind::Config,
                                Severity::Watch,
                                format!(
                                    "the reloaded keymap is unusable, so the running \
                                     configuration is unchanged: {error}"
                                ),
                            ));
                            let _ = ctx
                                .sender
                                .send(Event::ConfigReloaded(Err(error.to_string())));
                            return Flow::Continue;
                        }
                    };
                    ctx.settings = *reloaded.config.clone();
                    let history_rebuilt = state.reconfigure(&translated);
                    ctx.sampling.set_interval(ctx.settings.sampling.interval);
                    let policy_applied =
                        ctx.sampling.set_thresholds(thresholds_from(&ctx.settings));

                    for warning in reloaded.warnings {
                        state.push_notice(Notice::new(
                            NoticeKind::Config,
                            Severity::Watch,
                            warning,
                        ));
                    }
                    if history_rebuilt {
                        state.push_notice(Notice::new(
                            NoticeKind::Config,
                            Severity::Watch,
                            "the history settings changed, so retained samples were discarded"
                                .to_owned(),
                        ));
                    }
                    if !policy_applied {
                        state.push_notice(Notice::new(
                            NoticeKind::Config,
                            Severity::Critical,
                            "the pressure thresholds could not be handed to the sampler;                              it is still using the previous ones"
                                .to_owned(),
                        ));
                    }

                    let mut restart = non_reloadable(ctx.mouse_at_startup, &ctx.settings);
                    restart.extend(reloaded.non_reloadable.iter().map(|key| (*key).to_owned()));
                    let (severity, message) = if restart.is_empty() {
                        (Severity::Info, "reloaded the configuration".to_owned())
                    } else {
                        (
                            Severity::Watch,
                            format!(
                                "reloaded the configuration; these need a restart to take                                  effect: {}",
                                restart.join(", ")
                            ),
                        )
                    };
                    state.push_notice(Notice::new(NoticeKind::Config, severity, message));
                    let _ = ctx.sender.send(Event::ConfigReloaded(Ok(reloaded.config)));
                }
                Err(message) => {
                    // Reported here rather than left to the reducer: §10.1 keeps the
                    // configuration type out of `monitrs-tui`, so the reducer receives
                    // an opaque payload it can only redraw for. Saying the running
                    // configuration is untouched is the half a user needs — a refused
                    // reload is not a broken monitrs (§14.1), so this is a watch and
                    // not a critical.
                    state.push_notice(Notice::new(
                        NoticeKind::Config,
                        Severity::Watch,
                        format!(
                            "could not reload the configuration, so the running one is                              unchanged: {message}"
                        ),
                    ));
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
/// The reducer's view of a configuration.
///
/// One translation from [`Config`] to [`AppSettings`], used both to build the state
/// at startup and to hand a reloaded configuration to
/// [`AppState::reconfigure`]. Two copies of this mapping would be two chances for a
/// reload to apply a setting differently from the way startup applied it — and the
/// reload copy is the one that would go untested.
///
/// The fields it leaves at their defaults are the ones that are facts about the
/// session rather than settings: `started_at`, `size`, `view`, `env` and
/// `config_path`. The caller fills those in at startup and `reconfigure` ignores
/// them on reload.
///
/// `color_explicit` is threaded through because §5.2 makes `--color` on the command
/// line outrank both the file and the environment; a reload must not quietly hand
/// colour control back to `NO_COLOR`.
fn settings_to_app(settings: &Config, color_explicit: bool) -> Result<AppSettings, KeymapError> {
    Ok(AppSettings {
        sort: sort_from(settings),
        tree_mode: settings.processes.tree,
        filter: settings.processes.filter.clone(),
        only_user: None,
        hide_kernel_threads: !settings.display.show_kernel_threads,
        display: DisplaySettings {
            theme: ThemeId::from_name(&settings.display.theme).unwrap_or_default(),
            glyph_mode: settings.display.glyphs.into(),
            color_mode: settings.display.color.into(),
            color_explicit,
            byte_units: settings.display.units.into(),
        },
        history: history_config(settings),
        sample_interval: settings.sampling.interval,
        // Propagated rather than defaulted: falling back to the built-in keymap here
        // would take away the working bindings the user currently has, on the
        // strength of a file they have just broken. §12's atomic reload means an
        // unusable keymap invalidates the whole candidate.
        keymap: build_keymap(settings)?,
        sequence_timeout: monitrs_tui::keymap::DEFAULT_SEQUENCE_TIMEOUT,
        ..AppSettings::default()
    })
}

/// What a successful reload produced.
///
/// Separate from [`config::ReloadOutcome`] only in that the configuration is boxed
/// for the event payload; the fields it carries are the ones §12 requires to reach
/// the user — the parse warnings, and the keys that changed but cannot take effect.
struct Reloaded {
    config: Box<Config>,
    non_reloadable: Vec<&'static str>,
    warnings: Vec<String>,
}

/// Validates the configuration file against the running configuration (§12).
///
/// Returns `Ok` for anything that can be adopted, *including* a candidate whose
/// non-reloadable keys changed. That case used to come back as `Err`, which meant
/// the running configuration was left alone while the message said "reloaded" —
/// the one outcome §12 rules out, since a reload that appears to succeed and
/// changed nothing is indistinguishable from a bug. Adopting the reloadable part
/// and naming the rest is what "identified and explained" means.
fn reload(current: &Config, state: &AppState) -> Result<Reloaded, String> {
    let Some(path) = state.config_path() else {
        return Err(
            "no configuration file to reload; monitrs is using built-in defaults".to_owned(),
        );
    };
    match config::reload(current, path) {
        Ok(outcome) => Ok(Reloaded {
            config: Box::new(outcome.config),
            non_reloadable: outcome.non_reloadable,
            warnings: outcome.warnings,
        }),
        Err(error) => Err(error.to_string()),
    }
}

/// Settings that this *session* cannot adopt, whatever the file now says.
///
/// Only one, and it needs the session rather than the file to detect: mouse capture
/// is a terminal mode [`TerminalGuard`] set once at startup (§14.3), so the
/// comparison is against what the terminal is actually in — not against the
/// previous file, which a second reload would already have caught up with and
/// stopped reporting.
fn non_reloadable(mouse_at_startup: bool, settings: &Config) -> Vec<String> {
    if settings.display.mouse == mouse_at_startup {
        Vec::new()
    } else {
        vec!["display.mouse".to_owned()]
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

    match state.top_overlay() {
        Some(overlay) => draw_overlay(frame, area, state, presentation, overlay),
        // §2.2's spike attribution has no entry in the overlay stack: it is not
        // something the user opens, it is what selecting a historical sample *means*.
        // §5.6 shows it as the body of the Time Lens screen, so it is drawn whenever a
        // sample is selected and no other overlay is in the way.
        None => draw_attribution(frame, area, state, presentation),
    }
}

/// Draws the spike attribution panel for the selected historical sample (§2.2, §5.6).
///
/// Nothing is drawn while the timeline is live: there is no sample to attribute, and a
/// panel that appeared with empty contents would suggest the feature was broken rather
/// than idle.
fn draw_attribution(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    presentation: Presentation<'_>,
) {
    use monitrs_tui::views::overlays::SpikeAttributionOverlay;

    let timeline = state.timeline();
    if timeline.is_live() {
        return;
    }
    let ring = state.history();
    let view = timeline.view();
    let Some(sample) = view.selected(ring) else {
        return;
    };

    frame.render_widget(
        SpikeAttributionOverlay::for_sample(presentation, sample)
            .with_label("SPIKE ATTRIBUTION")
            .with_offset(view.format_offset(ring)),
        area,
    );
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

    use monitrs_core::units::ByteUnits;

    use crate::config::UnitsSetting;

    fn config_with_sort(sort: &str, descending: bool) -> Config {
        let mut settings = Config::default();
        settings.processes.sort = sort.to_owned();
        settings.processes.descending = descending;
        settings
    }

    /// A state built the way `run` builds it, for the reload tests.
    fn state_from(settings: &Config) -> AppState {
        let app = settings_to_app(settings, false).expect("the default keymap is usable");
        AppState::new(AppSettings {
            started_at: Instant::now(),
            size: (160, 48),
            env: TerminalEnv::empty(),
            config_path: Some(std::path::PathBuf::from("/nonexistent/monitrs.toml")),
            ..app
        })
    }

    /// A reload has to reach the state, not only the runtime's own copy.
    ///
    /// The bug this pins: `Effect::ReloadConfig` stored the new configuration in the
    /// runtime and sent an event the reducer could only redraw for, so every
    /// reloadable setting — the theme, the units, the ordering, the interval —
    /// changed everywhere except on the screen.
    #[test]
    fn a_reloaded_configuration_reaches_the_running_state() {
        let mut settings = Config::default();
        let mut state = state_from(&settings);
        assert_eq!(state.display().theme, ThemeId::DefaultDark);
        assert_eq!(state.sample_interval(), Duration::from_secs(1));
        assert_eq!(state.sort().key, ProcessSortKey::Cpu);

        // Presentation and ordering only, so the history ring is not involved.
        settings.display.theme = "high-contrast".to_owned();
        settings.display.units = UnitsSetting::Si;
        settings.processes.sort = "memory".to_owned();
        settings.processes.filter = "postgres".to_owned();

        let translated = settings_to_app(&settings, false).expect("still usable");
        let history_rebuilt = state.reconfigure(&translated);

        assert_eq!(state.display().theme, ThemeId::HighContrast);
        assert_eq!(state.display().byte_units, ByteUnits::Si);
        assert_eq!(state.sort().key, ProcessSortKey::Memory);
        assert_eq!(state.filter_text(), "postgres");
        assert!(
            !history_rebuilt,
            "changing the theme must not cost the user their retained samples"
        );
    }

    /// Changing the history settings is the one reload that costs the user data.
    ///
    /// Both the span and the *interval* do it: the ring's slot count is derived from
    /// the interval, so `interval = "2s"` reshapes it as surely as a new span does.
    /// That is worth a notice rather than a silent loss of the Time Lens (§8.5).
    #[test]
    fn a_reload_that_reshapes_the_history_ring_says_so() {
        for change in [
            |settings: &mut Config| settings.sampling.history = Duration::from_secs(600),
            |settings: &mut Config| settings.sampling.interval = Duration::from_millis(2_000),
        ] {
            let mut settings = Config::default();
            let mut state = state_from(&settings);
            change(&mut settings);
            let translated = settings_to_app(&settings, false).expect("usable");
            assert!(
                state.reconfigure(&translated),
                "a reshaped ring discards retained samples, and the caller has to be \
                 able to say so"
            );
        }

        // And the interval reaches the state, which is what the sampler is then told.
        let mut settings = Config::default();
        let mut state = state_from(&settings);
        settings.sampling.interval = Duration::from_millis(2_000);
        let translated = settings_to_app(&settings, false).expect("usable");
        let _ = state.reconfigure(&translated);
        assert_eq!(state.sample_interval(), Duration::from_millis(2_000));
    }

    /// The one setting a running session genuinely cannot adopt (§14.3).
    #[test]
    fn mouse_capture_is_reported_against_the_session_not_the_previous_file() {
        let mut settings = Config::default();
        settings.display.mouse = true;
        assert!(
            non_reloadable(true, &settings).is_empty(),
            "unchanged from startup, so there is nothing to report"
        );

        settings.display.mouse = false;
        assert_eq!(
            non_reloadable(true, &settings),
            vec!["display.mouse".to_owned()],
            "the terminal is still in the mode the guard set, so say so"
        );
        // And it keeps saying so. Comparing against the previous *file* would stop
        // reporting it after the first reload, while the terminal mode stayed wrong.
        assert_eq!(non_reloadable(true, &settings).len(), 1);
    }

    /// An unusable keymap invalidates the whole candidate (§12's atomic reload).
    ///
    /// A *conflict* is the unusable case: an unparseable chord is skipped with a
    /// warning by `build_keymap`, so it never reaches here.
    #[test]
    fn a_reload_whose_keymap_conflicts_translates_to_an_error() {
        let mut settings = Config::default();
        settings.keys.quit = Some(vec!["/".to_owned()]);
        assert!(
            settings_to_app(&settings, false).is_err(),
            "a conflicting binding must not be swapped for the built-in keymap, which \
             would take away the bindings the user still has"
        );
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
