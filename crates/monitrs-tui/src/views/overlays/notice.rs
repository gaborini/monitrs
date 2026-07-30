//! The error and notice overlay (§14.1, §21 M6).
//!
//! §14.2 forbids writing to stdout or stderr while the alternate screen is active, so
//! everything monitrs has to tell the user while running is recorded in
//! [`crate::app::NoticeLog`] and shown here: recoverable collector errors, permission refusals,
//! configuration problems, the clamped-configuration warnings §8.5 requires, export
//! outcomes, and the refusals §15.1 produces when a destructive action is declined.
//!
//! # Bounded, twice
//!
//! The log is already bounded to [`crate::app::MAX_NOTICES`] and merges
//! repeats, so a collector failing every second produces one counted line rather than
//! a hundred. This overlay bounds it again: at most [`MAX_VISIBLE_NOTICES`] lines,
//! each truncated to a fixed width, with a summary of what was left
//! out. The two bounds answer different risks — the log's bound is memory (§10.3) and
//! this one is legibility: a panel that filled the screen would hide the interface the
//! notices are about.
//!
//! # Dismissible, honestly
//!
//! §21 M6 asks for the overlay to be dismissible, and the key that dismisses it
//! belongs to whichever screen opens it — the notice log is not an
//! [`crate::app::Overlay`] on the stack, so `Esc` does not close it by itself.
//! [`NoticeOverlay::with_dismiss_hint`] therefore takes the label from the caller,
//! which has the keymap, and the footer is omitted entirely when no hint is given.
//! An overlay that invented a key would be telling the user to press something that
//! does nothing.
//!
//! The dismissal itself is [`crate::app::NoticeLog::clear`]. This overlay cannot call
//! it: a view renders state and never mutates it (§6.1), so emptying the log is the
//! reducer's job and drawing the result is this module's.

use monitrs_core::model::Severity;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::Notice;
use crate::theme::Token;
use crate::widgets::Presentation;
use crate::widgets::states::fit_within;

use super::frame::{Anchor, OverlayPanel};
use super::row::{muted, styled};

/// The panel title.
const TITLE: &str = "NOTICES";

/// The most notices shown at once.
///
/// Eight lines plus a border is a panel that can be read at a glance and still leaves
/// most of an 80×24 terminal visible. Anything older is summarised rather than shown:
/// the newest notice is the one the user is waiting for (§14.1).
pub const MAX_VISIBLE_NOTICES: usize = 8;

/// The widest a single message is rendered, in cells.
///
/// A parse error can quote a long path. Truncating keeps the panel dialog-shaped;
/// the marker makes the truncation visible so nothing reads as a complete message
/// when it is not (§5.4).
const MAX_MESSAGE_WIDTH: usize = 88;

/// The notice panel.
#[derive(Clone, Debug)]
pub struct NoticeOverlay<'a> {
    presentation: Presentation<'a>,
    notices: &'a [Notice],
    dropped: u64,
    dismiss_hint: Option<&'a str>,
}

impl<'a> NoticeOverlay<'a> {
    /// A panel over `notices`, oldest first as [`crate::app::NoticeLog`] keeps them.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, notices: &'a [Notice]) -> Self {
        Self {
            presentation,
            notices,
            dropped: 0,
            dismiss_hint: None,
        }
    }

    /// Records how many notices the log evicted before these.
    ///
    /// [`crate::app::NoticeLog::dropped`]. Saying that something was discarded is the
    /// difference between a bounded log and a lossy one.
    #[must_use]
    pub const fn with_dropped(mut self, dropped: u64) -> Self {
        self.dropped = dropped;
        self
    }

    /// Sets the key label that dismisses the panel.
    ///
    /// Supplied by the caller because only the caller knows the active keymap; the
    /// footer is omitted when it is not given, rather than naming a key that might
    /// not be bound (§12 allows `[keys]` overrides).
    #[must_use]
    pub const fn with_dismiss_hint(mut self, hint: &'a str) -> Self {
        self.dismiss_hint = Some(hint);
        self
    }

    /// The most severe notice on show, which is what a header indicator would use.
    #[must_use]
    pub fn worst(&self) -> Option<Severity> {
        self.notices.iter().map(|notice| notice.severity).max()
    }

    /// The notices that fit, newest last, each already carrying its symbol.
    ///
    /// [`Notice::render`] produces `! collector /proc/diskstats read failed (x12)` —
    /// symbol, kind, message and repeat count — so the §5.2 cue and the §14.1 taxonomy
    /// label come from the log rather than from a second formatting decision here.
    #[must_use]
    pub fn lines(&self) -> Vec<Line<'static>> {
        let glyphs = self.presentation.glyphs();
        let shown = self.notices.len().min(MAX_VISIBLE_NOTICES);
        let skipped = self.notices.len().saturating_sub(shown);
        let mut lines = Vec::with_capacity(shown.saturating_add(1));
        let hidden = skipped.saturating_add(usize::try_from(self.dropped).unwrap_or(usize::MAX));
        if hidden > 0 {
            let word = if hidden == 1 { "notice" } else { "notices" };
            lines.push(muted(
                self.presentation,
                &format!("{hidden} earlier {word} not shown"),
            ));
        }
        for notice in self.notices.iter().skip(skipped) {
            lines.push(styled(
                self.presentation,
                &fit_within(&notice.render(), MAX_MESSAGE_WIDTH, glyphs),
                notice.token(),
            ));
        }
        if lines.is_empty() {
            lines.push(muted(self.presentation, "nothing to report"));
        }
        lines
    }

    /// The footer, when the caller told us how the panel is dismissed.
    #[must_use]
    pub fn footer_lines(&self) -> Vec<Line<'static>> {
        match self.dismiss_hint {
            Some(hint) => vec![muted(self.presentation, &format!("{hint} dismiss"))],
            None => Vec::new(),
        }
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        let count = self.notices.len();
        let mut panel = OverlayPanel::new(self.presentation, TITLE)
            .anchored(Anchor::Center)
            .with_lines(self.lines())
            .with_footer(self.footer_lines());
        if count > 0 {
            let worst = self.worst().map_or("", Severity::label);
            panel = panel.with_trailing(format!("{count} {worst}"));
        }
        panel
    }

    /// The width the panel would like, borders included.
    #[must_use]
    pub fn desired_width(&self) -> u16 {
        self.panel().desired_width()
    }

    /// The height the panel would like, borders included.
    #[must_use]
    pub fn desired_height(&self) -> u16 {
        self.panel().desired_height()
    }
}

impl Widget for NoticeOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.panel().render(area, buf);
    }
}

/// The token a severity is drawn in, for a caller that wants to match the panel.
///
/// Delegates to [`Notice::token`] so there is one mapping rather than two; exposed
/// because the status line reinforces the same severity in the same colour.
#[must_use]
pub fn severity_token(severity: Severity) -> Token {
    match severity {
        Severity::Info => Token::Muted,
        Severity::Watch => Token::Watch,
        Severity::Critical => Token::Critical,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use monitrs_core::units::display_width;

    use super::*;
    use crate::app::{MAX_NOTICES, NoticeKind, NoticeLog};
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn log(notices: Vec<Notice>) -> NoticeLog {
        let mut log = NoticeLog::new();
        let now = Instant::now();
        for notice in notices {
            log.push(notice, now);
        }
        log
    }

    /// The §14.1 mix a real session produces: a collector error, a permission
    /// refusal, a configuration clamp, and a declined action.
    fn taxonomy() -> NoticeLog {
        log(vec![
            Notice::watch(NoticeKind::Collector, "/proc/diskstats read failed"),
            Notice::watch(
                NoticeKind::Permission,
                "per-process I/O is not permitted at this privilege level",
            ),
            Notice::watch(
                NoticeKind::Config,
                "history.duration 24h was clamped to 1h; the memory budget would be exceeded",
            ),
            Notice::info(NoticeKind::Export, "wrote 12 KiB to /tmp/monitrs.json"),
            Notice::critical(NoticeKind::Terminal, "the terminal reported a write error"),
        ])
    }

    fn render(overlay: NoticeOverlay<'_>, width: u16, height: u16) -> String {
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

    fn full(overlay: NoticeOverlay<'_>) -> String {
        let width = overlay.desired_width();
        let height = overlay.desired_height();
        render(overlay, width, height)
    }

    #[test]
    fn every_class_of_notice_is_rendered_with_its_label_and_symbol() {
        let log = taxonomy();
        let text = full(NoticeOverlay::new(presentation(), log.as_slice()));

        assert!(text.contains("collector"), "{text}");
        assert!(text.contains("permission"), "{text}");
        assert!(text.contains("config"), "{text}");
        assert!(text.contains("export"), "{text}");
        assert!(text.contains("terminal"), "{text}");
        // §5.2: severity is a character as well as a colour.
        assert!(text.contains("! collector"), "{text}");
        assert!(text.contains("X terminal"), "{text}");
        assert!(text.contains(". export"), "{text}");
    }

    #[test]
    fn a_clamped_configuration_warning_is_shown_verbatim() {
        // §8.5 and §21 M6: the user must learn that a configured value was adjusted.
        let log = taxonomy();
        let text = full(NoticeOverlay::new(presentation(), log.as_slice()));
        assert!(text.contains("was clamped to 1h"), "{text}");
    }

    #[test]
    fn the_panel_is_bounded_however_long_the_log_is() {
        let mut entries = Vec::new();
        for index in 0..MAX_NOTICES {
            entries.push(Notice::info(
                NoticeKind::Collector,
                format!("message {index}"),
            ));
        }
        let log = log(entries);
        let overlay = NoticeOverlay::new(presentation(), log.as_slice());
        assert!(overlay.lines().len() <= MAX_VISIBLE_NOTICES + 1);
        let text = full(overlay);
        assert!(text.contains("earlier notices not shown"), "{text}");
        assert!(
            text.contains(&format!("message {}", MAX_NOTICES - 1)),
            "the newest notice must be visible:\n{text}"
        );
    }

    #[test]
    fn a_very_long_message_is_truncated_rather_than_widening_the_panel() {
        let log = log(vec![Notice::watch(
            NoticeKind::Config,
            format!("unreadable path {}", "/very-long-segment".repeat(20)),
        )]);
        let overlay = NoticeOverlay::new(presentation(), log.as_slice());
        for line in overlay.lines() {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(display_width(&text) <= MAX_MESSAGE_WIDTH, "{text:?}");
        }
        assert!(full(NoticeOverlay::new(presentation(), log.as_slice())).contains("..."));
    }

    #[test]
    fn a_repeated_notice_is_counted_rather_than_repeated() {
        let mut log = NoticeLog::new();
        let now = Instant::now();
        for _ in 0..12 {
            log.push(
                Notice::watch(NoticeKind::Collector, "sample took too long"),
                now,
            );
        }
        let text = full(NoticeOverlay::new(presentation(), log.as_slice()));
        assert!(text.contains("(x12)"), "{text}");
        assert_eq!(
            text.lines()
                .filter(|line| line.contains("sample took too long"))
                .count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn evicted_notices_are_accounted_for_rather_than_forgotten() {
        let log = log(vec![Notice::info(NoticeKind::Export, "wrote 4 KiB")]);
        let text = full(NoticeOverlay::new(presentation(), log.as_slice()).with_dropped(7));
        assert!(text.contains("7 earlier notices not shown"), "{text}");
    }

    #[test]
    fn the_dismiss_hint_appears_only_when_the_caller_supplies_one() {
        let log = taxonomy();
        let bare = full(NoticeOverlay::new(presentation(), log.as_slice()));
        assert!(!bare.contains("dismiss"), "{bare}");
        let hinted =
            full(NoticeOverlay::new(presentation(), log.as_slice()).with_dismiss_hint("Esc"));
        assert!(hinted.contains("Esc dismiss"), "{hinted}");
    }

    #[test]
    fn the_header_names_the_worst_severity_on_show() {
        let log = taxonomy();
        let overlay = NoticeOverlay::new(presentation(), log.as_slice());
        assert_eq!(overlay.worst(), Some(Severity::Critical));
        assert!(full(overlay).contains("critical"));
    }

    #[test]
    fn an_empty_log_says_so_rather_than_rendering_an_empty_box() {
        let overlay = NoticeOverlay::new(presentation(), &[]);
        assert_eq!(overlay.worst(), None);
        assert!(full(overlay).contains("nothing to report"));
    }

    #[test]
    fn the_severity_token_matches_the_notices_own_token() {
        for (severity, notice) in [
            (Severity::Info, Notice::info(NoticeKind::Export, "x")),
            (Severity::Watch, Notice::watch(NoticeKind::Config, "x")),
            (
                Severity::Critical,
                Notice::critical(NoticeKind::Terminal, "x"),
            ),
        ] {
            assert_eq!(severity_token(severity), notice.token());
        }
    }

    #[test]
    fn the_panel_degrades_and_never_panics() {
        let log = taxonomy();
        for (width, height) in [(80u16, 24u16), (60, 16), (20, 4), (1, 1), (0, 0)] {
            let overlay = NoticeOverlay::new(presentation(), log.as_slice())
                .with_dismiss_hint("Esc")
                .with_dropped(3);
            let text = render(overlay, width, height);
            for row in text.lines() {
                assert!(display_width(row) <= usize::from(width), "{row:?}");
            }
        }
    }
}
