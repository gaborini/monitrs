//! The process-action confirmation dialog (§6.2, §15.1).
//!
//! This is the most safety-critical rendering in monitrs, and it is deliberately
//! the least clever. It renders a [`ProcessActionStage`] — the state machine
//! [`crate::app::reduce`] already owns — and does nothing else. It cannot advance
//! the chain, cannot construct a [`crate::action::PendingProcessAction`], and cannot
//! reach [`crate::action::Effect::SignalProcess`]: that effect has exactly one
//! constructor, [`crate::action::PendingProcessAction::into_effect`], and it lives
//! in a module this one only reads from.
//!
//! # What §6.2 requires on screen, and where it is
//!
//! | Requirement | Where |
//! |---|---|
//! | process name | pinned `NAME` row |
//! | PID | pinned `PID` row, and the identity row beside it |
//! | start time or age | pinned `STARTED` and `AGE` rows |
//! | user | pinned `USER` row |
//! | command | pinned `COMMAND` row |
//! | requested action | body `ACTION` row, or the highlighted choice |
//! | consequences | body `CONSEQUENCE` row |
//! | explicit confirmation key | footer, from [`ConfirmationKind::key_hint`] |
//!
//! The identity row shows the start key as well as the PID because §26 is blunt
//! that *a PID is not an identity*. The user is about to authorise something
//! irreversible against a number that the kernel reuses; the value that makes it
//! unambiguous belongs on screen next to it.
//!
//! # `SIGKILL`
//!
//! §9.2 orders it last, which [`SignalKind::DIALOG_ORDER`] does, so the dialog
//! simply renders that order and cannot get it wrong. §15.1 wants it *marked* and
//! wants its confirmation *distinct from ordinary Enter*: the row carries the
//! critical state glyph and the `forceful` word, and the footer prints
//! [`ConfirmationKind::Forceful`]'s hint together with an explicit statement that
//! Enter will not do. Both facts are read from the action, never re-derived here,
//! so the dialog cannot promise a key that [`ConfirmationKind::accepts`] rejects.
//!
//! # A process that cannot be signalled
//!
//! A zombie has already exited: signalling it is a no-op, and §15.1 requires
//! already-exited processes to be *clearly reported*. The reducer refuses to open
//! the dialog for one — but a process can become a zombie while the dialog is open,
//! and it can vanish from the table entirely, so this is a live case rather than a
//! defensive one. In both situations the dialog states plainly that nothing can be
//! delivered and **withholds the confirmation hint**, so the affordance that says
//! "this will work" is not offered for something that will not.
//!
//! Absence from the displayed snapshot is conclusive here: §15.1 disables process
//! actions unless the timeline is live, so while this dialog is open the displayed
//! snapshot *is* the live one.

use monitrs_core::model::{ProcessSnapshot, ProcessState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::action::{ConfirmationKind, PendingProcessAction, SignalKind};
use crate::app::{MAX_NICE, MIN_NICE, OverlayKind, ProcessActionStage};
use crate::glyphs::Glyph;
use crate::theme::Token;
use crate::widgets::Presentation;
use crate::widgets::states::{describe, describe_age, describe_display};

use super::clock::format_timestamp;
use super::frame::{Anchor, OverlayPanel};
use super::row::{blank, heading, metric_field, muted, plain, styled, text_field};

/// The key that closes an overlay without acting.
///
/// `Esc` is bound to [`crate::action::Action::CancelOverlay`] in *every* mode
/// (§6.2), which is why it can be named here: a per-mode `[keys]` override cannot
/// take it away without removing the only universal cancel. Pinned by
/// `the_keys_this_dialog_names_are_really_bound`.
const CANCEL_HINT: &str = "Esc";

/// The confirmation dialog for a signal or a renice.
#[derive(Clone, Debug)]
pub struct ProcessActionOverlay<'a> {
    presentation: Presentation<'a>,
    stage: ProcessActionStage,
    process: Option<&'a ProcessSnapshot>,
}

impl<'a> ProcessActionOverlay<'a> {
    /// A dialog for `stage`.
    ///
    /// `process` is the row the stage's identity resolves to in the *displayed*
    /// snapshot, or `None` when it no longer resolves to one. Passing `None` is not
    /// an error state to be hidden: it is the "already exited" case §15.1 requires
    /// to be reported, and the dialog renders it as such.
    #[must_use]
    pub const fn new(
        presentation: Presentation<'a>,
        stage: ProcessActionStage,
        process: Option<&'a ProcessSnapshot>,
    ) -> Self {
        Self {
            presentation,
            stage,
            process,
        }
    }

    /// Whether the action this dialog describes could actually take effect.
    ///
    /// False for a vanished process and for one the kernel has already reaped, which
    /// is what suppresses the confirmation hint (§15.1).
    #[must_use]
    pub fn is_deliverable(&self) -> bool {
        self.process
            .is_some_and(|process| process.state.is_signalable())
    }

    /// Why the action cannot be delivered, if it cannot.
    ///
    /// One sentence, prefixed with the critical state character so the refusal
    /// survives with colour off (§5.2).
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        let critical = self.presentation.glyph(Glyph::StateCritical);
        let pid = self.stage.identity().pid;
        match self.process {
            None => Some(format!(
                "{critical} PID {pid} is no longer in the process table; nothing can be sent to it"
            )),
            Some(process) if !process.state.is_signalable() => Some(format!(
                "{critical} PID {pid} has already exited ({}); a signal would have no effect",
                process.state.label()
            )),
            Some(_) => None,
        }
    }

    /// The identity block, which never scrolls.
    ///
    /// Every §6.2 field is here, and each one that the OS may withhold goes through
    /// [`crate::widgets::states`] so an unmeasured field reads as unmeasured rather
    /// than as blank.
    #[must_use]
    pub fn identity_lines(&self) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let identity = self.stage.identity();
        let mut lines = vec![
            text_field(
                presentation,
                "NAME",
                self.process.map_or("n/a", |process| &*process.name),
                Token::Text,
            ),
            text_field(presentation, "PID", &identity.pid.to_string(), Token::Text),
            // §26: a PID is not an identity. The start key is what makes this row
            // refer to one process rather than to whatever holds the PID next.
            text_field(
                presentation,
                "IDENTITY",
                &format!("pid {} start key {}", identity.pid, identity.start_key),
                Token::Muted,
            ),
        ];
        match self.process {
            Some(process) => {
                lines.push(metric_field(
                    presentation,
                    "USER",
                    &describe(&process.user, |user| user.display_name()),
                ));
                lines.push(text_field(
                    presentation,
                    "STATE",
                    process.state.label(),
                    state_token(process.state),
                ));
                lines.push(metric_field(
                    presentation,
                    "STARTED",
                    &describe(&process.started_at, |started| format_timestamp(*started)),
                ));
                lines.push(metric_field(
                    presentation,
                    "AGE",
                    &describe_age(&process.age),
                ));
                lines.push(metric_field(
                    presentation,
                    "THREADS",
                    &describe_display(&process.threads),
                ));
                lines.push(text_field(
                    presentation,
                    "COMMAND",
                    process.command_or_name(),
                    Token::Text,
                ));
            }
            None => lines.push(muted(
                presentation,
                "the rest of this process's detail is no longer available",
            )),
        }
        lines
    }

    /// The scrolling body: the choice being made, or the action being confirmed.
    #[must_use]
    pub fn action_lines(&self) -> Vec<Line<'static>> {
        match self.stage {
            ProcessActionStage::ChooseSignal { cursor, .. } => self.signal_choices(cursor),
            ProcessActionStage::ChooseNice { nice, .. } => self.nice_choice(nice),
            ProcessActionStage::Confirm(pending) => self.confirmation(pending),
        }
    }

    /// The §9.2 signal menu, `SIGKILL` last and marked.
    fn signal_choices(&self, cursor: usize) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let mut lines = vec![heading(presentation, "CHOOSE A SIGNAL")];
        for (index, signal) in SignalKind::DIALOG_ORDER.into_iter().enumerate() {
            let marker = if index == cursor {
                presentation.glyph(Glyph::SelectionMarker)
            } else {
                presentation.glyph(Glyph::SelectionBlank)
            };
            let mark = if signal.is_forceful() {
                format!("{} forceful ", presentation.glyph(Glyph::StateCritical))
            } else {
                String::new()
            };
            let token = if signal.is_forceful() {
                Token::Critical
            } else if index == cursor {
                Token::Accent
            } else {
                Token::Text
            };
            lines.push(styled(
                presentation,
                &format!(
                    "{marker} {:<8} signal {:<2}  {mark}{}",
                    signal.name(),
                    signal.number(),
                    signal.consequence()
                ),
                token,
            ));
        }
        lines.push(blank());
        lines.push(muted(
            presentation,
            "j / k move the choice; nothing is sent until it is confirmed",
        ));
        lines
    }

    /// The renice dialog (§6.2 `R`).
    fn nice_choice(&self, nice: i8) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        vec![
            heading(presentation, "CHOOSE A PRIORITY"),
            text_field(presentation, "NICE", &nice.to_string(), Token::Accent),
            text_field(
                presentation,
                "RANGE",
                &format!("{MIN_NICE} to {MAX_NICE}"),
                Token::Muted,
            ),
            blank(),
            muted(
                presentation,
                "j / k adjust the value; lowering it may need privileges monitrs will not escalate to",
            ),
        ]
    }

    /// The resolved action awaiting its confirmation (§15.1).
    fn confirmation(&self, pending: PendingProcessAction) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let forceful = pending.is_forceful();
        let mut lines = vec![
            heading(presentation, "REQUESTED ACTION"),
            text_field(
                presentation,
                "ACTION",
                &action_label(pending),
                if forceful {
                    Token::Critical
                } else {
                    Token::Accent
                },
            ),
            // On its own line, indented, rather than in a labelled field: the longest
            // consequence sentence is seventy cells and a fourteen-cell label would
            // push it past an 80-column panel. This is the sentence the user is
            // consenting to, so it is the one that must read in full (§6.2).
            muted(presentation, "CONSEQUENCE"),
            plain(presentation, &format!("  {}", pending.consequence())),
        ];
        if forceful {
            lines.push(styled(
                presentation,
                &format!(
                    "{} this signal cannot be caught, blocked or ignored",
                    presentation.glyph(Glyph::StateCritical)
                ),
                Token::Critical,
            ));
        }
        lines.push(blank());
        // Two lines rather than one long one: a sentence clipped by a narrow panel has
        // no truncation marker to warn the reader, so it reads as complete (§5.4).
        lines.push(muted(
            presentation,
            "the identity above is re-read immediately before delivery;",
        ));
        lines.push(muted(
            presentation,
            "if this PID has become another process the action is abandoned",
        ));
        lines
    }

    /// The footer: the explicit confirmation key §6.2 requires, or the refusal.
    #[must_use]
    pub fn footer_lines(&self) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        if let Some(refusal) = self.refusal() {
            return vec![
                styled(presentation, &refusal, Token::Critical),
                plain(presentation, &format!("{CANCEL_HINT} close")),
            ];
        }
        let pending = self.stage.resolved();
        // A choosing stage is advanced by the ordinary key whichever signal is
        // highlighted — the forceful key is demanded only once the action is
        // resolved and about to be delivered. Naming `Y` here would teach the user
        // to reach for it a step early, which is the opposite of what §15.1 wants.
        let (confirmation, verb) = match self.stage {
            ProcessActionStage::ChooseSignal { .. } | ProcessActionStage::ChooseNice { .. } => {
                (ConfirmationKind::Ordinary, "review".to_owned())
            }
            ProcessActionStage::Confirm(_) => (
                pending.confirmation(),
                format!("send {}", action_label(pending)),
            ),
        };
        let mut lines = vec![plain(
            presentation,
            &format!("{} {verb}   {CANCEL_HINT} cancel", confirmation.key_hint()),
        )];
        if confirmation == ConfirmationKind::Forceful {
            lines.push(styled(
                presentation,
                &format!(
                    "{} Enter will not confirm a forceful action; {} is required",
                    presentation.glyph(Glyph::StateWatch),
                    confirmation.key_hint()
                ),
                Token::Watch,
            ));
        }
        lines
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        OverlayPanel::new(self.presentation, OverlayKind::ProcessAction.title())
            .anchored(Anchor::Center)
            .with_pinned(self.identity_lines())
            .with_lines(self.action_lines())
            .with_footer(self.footer_lines())
    }

    /// The width the dialog would like, borders included.
    #[must_use]
    pub fn desired_width(&self) -> u16 {
        self.panel().desired_width()
    }

    /// The height the dialog would like, borders included.
    #[must_use]
    pub fn desired_height(&self) -> u16 {
        self.panel().desired_height()
    }
}

impl Widget for ProcessActionOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.panel().render(area, buf);
    }
}

/// How an action is named on screen: `SIGKILL (signal 9)`, `renice to 5`.
fn action_label(pending: PendingProcessAction) -> String {
    match pending {
        PendingProcessAction::Signal { signal, .. } => {
            format!("{} (signal {})", signal.name(), signal.number())
        }
        PendingProcessAction::Renice { nice, .. } => format!("renice to {nice}"),
    }
}

/// The token a process state is drawn in.
///
/// §7.2 requires zombie and uninterruptible sleep to be visibly distinct, and in
/// this dialog the state is the difference between an action that will work and one
/// that cannot. The redundant character comes from [`ProcessActionOverlay::refusal`],
/// which is always rendered alongside a non-signalable state.
const fn state_token(state: ProcessState) -> Token {
    if state.is_signalable() {
        if state.is_notable() {
            Token::Watch
        } else {
            Token::Text
        }
    } else {
        Token::Critical
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::time::SystemTime;

    use monitrs_core::model::{
        MetricState, ProcessIdentity, ProcessIo, ProcessMemory, UserIdentity,
    };
    use monitrs_core::units::Percent;

    use super::*;
    use crate::action::Action;
    use crate::glyphs::GlyphSet;
    use crate::keymap::{InputMode, Keymap};
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn identity() -> ProcessIdentity {
        ProcessIdentity::new(31_842, 900_100)
    }

    fn process(state: ProcessState) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: identity(),
            parent_pid: Some(1),
            name: "rustc".into(),
            command: "cargo build --release".into(),
            exe: Some("/usr/bin/rustc".into()),
            user: MetricState::Available(UserIdentity {
                uid: 501,
                name: Some("gabor".into()),
            }),
            state,
            cpu: MetricState::Available(Percent::new(287.0).expect("valid")),
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Available(9),
            age: MetricState::Available(Duration::from_secs(43)),
            started_at: MetricState::Available(
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_363_247),
            ),
            is_kernel_thread: false,
        }
    }

    fn render(overlay: ProcessActionOverlay<'_>, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        overlay.render(area, &mut buffer);
        (0..height)
            .map(|y| {
                let row: String = (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                    .collect();
                format!("{row}\n")
            })
            .collect()
    }

    /// Renders at a size that comfortably fits the whole dialog.
    fn full(overlay: ProcessActionOverlay<'_>) -> String {
        let width = overlay.desired_width();
        let height = overlay.desired_height();
        render(overlay, width, height)
    }

    fn confirm(signal: SignalKind) -> ProcessActionStage {
        ProcessActionStage::Confirm(PendingProcessAction::Signal {
            identity: identity(),
            signal,
        })
    }

    #[test]
    fn the_dialog_shows_every_field_the_specification_requires() {
        // §6.2's list, checked one clause at a time against the rendered buffer.
        let running = process(ProcessState::Running);
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Term), Some(&running));
        let text = full(overlay);

        assert!(text.contains("rustc"), "process name missing:\n{text}");
        assert!(text.contains("31842"), "PID missing:\n{text}");
        assert!(
            text.contains("2026-07-29 22:14:07 UTC"),
            "start time missing:\n{text}"
        );
        assert!(text.contains("00:43"), "age missing:\n{text}");
        assert!(text.contains("gabor"), "user missing:\n{text}");
        assert!(
            text.contains("cargo build --release"),
            "command missing:\n{text}"
        );
        assert!(text.contains("SIGTERM"), "action missing:\n{text}");
        assert!(
            text.contains("asks the process to exit"),
            "consequence missing:\n{text}"
        );
        assert!(
            text.contains("Enter"),
            "the explicit confirmation key is missing:\n{text}"
        );
        assert!(
            text.contains("900100"),
            "§26: the start key belongs beside the PID:\n{text}"
        );
    }

    #[test]
    fn sigkill_is_last_in_the_menu_and_marked_forceful() {
        let running = process(ProcessState::Running);
        let overlay = ProcessActionOverlay::new(
            presentation(),
            ProcessActionStage::ChooseSignal {
                identity: identity(),
                cursor: 0,
            },
            Some(&running),
        );
        let text = full(overlay);

        let position = |name: &str| text.find(name);
        let kill = position("SIGKILL").expect("SIGKILL is offered");
        for other in ["SIGTERM", "SIGINT", "SIGHUP"] {
            let at = position(other).unwrap_or_else(|| panic!("{other} is offered"));
            assert!(at < kill, "§9.2 puts SIGKILL last, but {other} follows it");
        }
        let kill_line = text
            .lines()
            .find(|line| line.contains("SIGKILL"))
            .expect("the SIGKILL row");
        assert!(kill_line.contains("forceful"), "{kill_line}");
        assert!(
            kill_line.contains('X'),
            "the forceful mark must survive without colour: {kill_line}"
        );
    }

    #[test]
    fn a_forceful_confirmation_names_a_key_that_is_not_enter() {
        let running = process(ProcessState::Running);
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Kill), Some(&running));
        let text = full(overlay);

        assert!(text.contains("SIGKILL"), "{text}");
        assert!(
            text.contains(ConfirmationKind::Forceful.key_hint()),
            "the forceful key is missing:\n{text}"
        );
        assert!(
            text.contains("Enter will not confirm"),
            "§15.1: the dialog must say Enter is not enough:\n{text}"
        );
    }

    #[test]
    fn an_ordinary_confirmation_does_not_demand_the_forceful_key() {
        let running = process(ProcessState::Running);
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Term), Some(&running));
        let text = full(overlay);
        assert!(!text.contains("Enter will not confirm"), "{text}");
    }

    #[test]
    fn the_confirmation_hint_agrees_with_what_the_action_actually_accepts() {
        // If these ever disagree the dialog is lying about how to confirm a
        // destructive action, which is the failure §6.2's "explicit confirmation
        // key" clause exists to prevent.
        let running = process(ProcessState::Running);
        for signal in SignalKind::DIALOG_ORDER {
            let stage = confirm(signal);
            let overlay = ProcessActionOverlay::new(presentation(), stage, Some(&running));
            let text = full(overlay);
            let expected = stage.resolved().confirmation();
            assert!(
                text.contains(expected.key_hint()),
                "{} names no usable confirmation key:\n{text}",
                signal.name()
            );
            if expected == ConfirmationKind::Forceful {
                assert!(
                    !expected.accepts(&Action::ConfirmPendingAction),
                    "the hint would be wrong about Enter"
                );
            }
        }
    }

    #[test]
    fn a_zombie_is_reported_rather_than_pretended_at() {
        // §15.1: already-exited processes are clearly reported. The dialog says so
        // and withholds the affordance that would imply the signal will land.
        let zombie = process(ProcessState::Zombie);
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Term), Some(&zombie));
        assert!(!overlay.is_deliverable());
        let text = full(overlay);

        assert!(text.contains("already exited"), "{text}");
        assert!(text.contains("zombie"), "{text}");
        assert!(
            text.contains("would have no effect"),
            "the dialog must not imply the signal lands:\n{text}"
        );
        assert!(
            !text.contains("Enter send"),
            "a confirmation was offered for an unsignalable process:\n{text}"
        );
        assert!(text.contains("Esc close"), "{text}");
    }

    #[test]
    fn a_zombie_proposed_for_sigkill_is_refused_just_as_plainly() {
        let zombie = process(ProcessState::Zombie);
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Kill), Some(&zombie));
        let text = full(overlay);
        assert!(text.contains("already exited"), "{text}");
        assert!(
            !text.contains("Y send"),
            "the forceful confirmation was offered for a reaped process:\n{text}"
        );
    }

    #[test]
    fn a_vanished_process_is_reported_rather_than_rendered_as_blanks() {
        let overlay = ProcessActionOverlay::new(presentation(), confirm(SignalKind::Term), None);
        assert!(!overlay.is_deliverable());
        let text = full(overlay);
        assert!(text.contains("no longer in the process table"), "{text}");
        assert!(text.contains("31842"), "the PID is still named:\n{text}");
        assert!(!text.contains("Enter send"), "{text}");
    }

    #[test]
    fn a_dead_process_is_also_refused() {
        let dead = process(ProcessState::Dead);
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Term), Some(&dead));
        assert!(!overlay.is_deliverable());
        assert!(full(overlay).contains("already exited"));
    }

    #[test]
    fn an_unmeasured_field_reads_honestly_rather_than_as_a_blank() {
        let mut unknown = process(ProcessState::Running);
        unknown.user = MetricState::PermissionDenied;
        unknown.started_at = MetricState::Unsupported;
        unknown.age = MetricState::WarmingUp;
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Term), Some(&unknown));
        let text = full(overlay);

        assert!(text.contains("permission denied"), "{text}");
        assert!(text.contains("n/a"), "{text}");
        assert!(text.contains("warming up"), "{text}");
        assert!(
            text.contains("! permission denied"),
            "the §5.2 symbol is missing:\n{text}"
        );
    }

    #[test]
    fn the_renice_dialog_states_its_range_and_refuses_to_escalate() {
        let running = process(ProcessState::Running);
        let overlay = ProcessActionOverlay::new(
            presentation(),
            ProcessActionStage::ChooseNice {
                identity: identity(),
                nice: 5,
            },
            Some(&running),
        );
        let text = full(overlay);
        assert!(text.contains("NICE"), "{text}");
        assert!(text.contains("-20 to 19"), "{text}");
        assert!(text.contains("will not escalate"), "{text}");
    }

    #[test]
    fn a_choosing_stage_says_the_key_only_moves_to_a_review() {
        let running = process(ProcessState::Running);
        let overlay = ProcessActionOverlay::new(
            presentation(),
            ProcessActionStage::ChooseSignal {
                identity: identity(),
                cursor: 3,
            },
            Some(&running),
        );
        let text = full(overlay);
        assert!(text.contains("Enter review"), "{text}");
        assert!(
            text.contains("nothing is sent until it is confirmed"),
            "{text}"
        );
    }

    #[test]
    fn the_highlighted_choice_is_marked_without_relying_on_colour() {
        let running = process(ProcessState::Running);
        let overlay = ProcessActionOverlay::new(
            presentation(),
            ProcessActionStage::ChooseSignal {
                identity: identity(),
                cursor: 2,
            },
            Some(&running),
        );
        let text = full(overlay);
        let marked: Vec<&str> = text.lines().filter(|line| line.contains("> SIG")).collect();
        assert_eq!(marked.len(), 1, "{text}");
        assert!(
            marked.first().is_some_and(|line| line.contains("SIGHUP")),
            "{marked:?}"
        );
    }

    #[test]
    fn the_keys_this_dialog_names_are_really_bound() {
        // The dialog prints `Esc`, and the confirmation hints; all three have to be
        // live in `ConfirmProcessAction` mode or the dialog is describing a keymap
        // that does not exist.
        let keymap = Keymap::builtin();
        let bound = |label: &str| {
            keymap
                .bindings_for_mode(InputMode::ConfirmProcessAction)
                .any(|binding| binding.chord.label() == label)
        };
        assert!(bound(CANCEL_HINT), "`{CANCEL_HINT}` is not bound");
        assert!(bound(ConfirmationKind::Ordinary.key_hint()));
        assert!(bound(ConfirmationKind::Forceful.key_hint()));
        assert!(bound("j"), "the choice hint names j");
        assert!(bound("k"), "the choice hint names k");
    }

    #[test]
    fn the_confirmation_key_survives_a_terminal_too_short_for_the_dialog() {
        // §5.7's floor is 60x16; the key hint must be on screen even below it.
        let running = process(ProcessState::Running);
        for (width, height) in [(80u16, 24u16), (60, 16), (44, 6), (30, 3)] {
            let overlay = ProcessActionOverlay::new(
                presentation(),
                confirm(SignalKind::Kill),
                Some(&running),
            );
            let text = render(overlay, width, height);
            assert!(
                text.contains(ConfirmationKind::Forceful.key_hint()),
                "{width}x{height} lost the confirmation key:\n{text}"
            );
        }
    }

    #[test]
    fn a_zero_area_dialog_draws_nothing_and_never_panics() {
        let running = process(ProcessState::Running);
        for (width, height) in [(0u16, 0u16), (0, 24), (80, 0), (1, 1), (2, 2)] {
            let overlay = ProcessActionOverlay::new(
                presentation(),
                confirm(SignalKind::Term),
                Some(&running),
            );
            let _ = render(overlay, width, height);
        }
    }

    #[test]
    fn a_notable_but_signalable_state_is_flagged_without_being_refused() {
        let blocked = process(ProcessState::UninterruptibleSleep);
        let overlay =
            ProcessActionOverlay::new(presentation(), confirm(SignalKind::Term), Some(&blocked));
        assert!(overlay.is_deliverable());
        assert_eq!(
            state_token(ProcessState::UninterruptibleSleep),
            Token::Watch
        );
        assert_eq!(state_token(ProcessState::Running), Token::Text);
        assert_eq!(state_token(ProcessState::Zombie), Token::Critical);
        assert!(full(overlay).contains("uninterruptible sleep"));
    }
}
