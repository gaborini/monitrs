//! The bounded notice log: what monitrs has to tell the user (§14.1).
//!
//! §14.1 separates the kinds of failure a system monitor produces, and the ones
//! that happen *while the interface is running* cannot be printed: stdout belongs
//! to the alternate screen (§14.2). They are recorded here and rendered in the
//! status area instead.
//!
//! Three properties are deliberate:
//!
//! * **Bounded.** §10.3 forbids unbounded accumulation anywhere. The log keeps
//!   the most recent [`MAX_NOTICES`] entries — the opposite of
//!   [`monitrs_core::model::CollectorHealth::record_issue`], which keeps the
//!   *first* distinct failures because it is a root-cause record. A notice is a
//!   message to the user, and the newest one is the one they are waiting for.
//! * **Repeats are counted, not repeated.** A collector failing every second must
//!   not push a hundred identical lines through a four-line panel.
//! * **Every notice carries a symbol.** §5.2 forbids colour from being the only
//!   indicator, so severity is expressed as a character too, taken from
//!   [`Severity::symbol`] so that a `watch` reads identically here and in the
//!   Pressure Radar.

use std::time::Instant;

use monitrs_core::model::Severity;

use crate::theme::Token;

/// The most recent notices retained.
///
/// Four lines is what the §5.5 status area can show without stealing rows from
/// the process table; the log keeps a few more so a user who has just been away
/// can scroll back through what happened.
pub const MAX_NOTICES: usize = 16;

/// Which part of §14.1's taxonomy a notice belongs to.
///
/// Fatal startup errors are absent on purpose: they happen before there is an
/// interface to show them in, so the binary prints them and exits (§14.1).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoticeKind {
    /// A recoverable collector error.
    Collector,
    /// The OS refused a read at our privilege level.
    Permission,
    /// A configuration problem, or the outcome of a reload.
    Config,
    /// The outcome of, or a refusal to start, a process action (§15.1).
    ProcessAction,
    /// A snapshot export.
    Export,
    /// A terminal error.
    Terminal,
    /// A request the interface declined to perform, such as a destructive action
    /// while the timeline is not live.
    ///
    /// Not one of §14.1's error classes — nothing failed — but the user pressed a
    /// key and is owed an explanation rather than silence.
    Interaction,
}

impl NoticeKind {
    /// Every kind, for exhaustiveness tests and for the Inspect screen.
    pub const ALL: [Self; 7] = [
        Self::Collector,
        Self::Permission,
        Self::Config,
        Self::ProcessAction,
        Self::Export,
        Self::Terminal,
        Self::Interaction,
    ];

    /// The lower-case label shown in front of the message.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Collector => "collector",
            Self::Permission => "permission",
            Self::Config => "config",
            Self::ProcessAction => "process",
            Self::Export => "export",
            Self::Terminal => "terminal",
            Self::Interaction => "input",
        }
    }
}

/// One thing the user should know.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    /// Which taxonomy class it belongs to.
    pub kind: NoticeKind,
    /// How serious it is.
    pub severity: Severity,
    /// The message. One sentence, no trailing period, no leading capital unless
    /// it is a proper noun — the renderer prefixes it with the kind label.
    pub message: String,
    /// How many times this exact notice has been recorded.
    pub occurrences: u32,
    /// Monotonic time of the most recent occurrence, so the renderer can age or
    /// expire it (§8.1: never the wall clock for ordering).
    pub last_seen: Option<Instant>,
}

impl Notice {
    /// A notice with no timestamp yet; [`NoticeLog::push`] stamps it.
    #[must_use]
    pub fn new(kind: NoticeKind, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            kind,
            severity,
            message: message.into(),
            occurrences: 1,
            last_seen: None,
        }
    }

    /// An informational notice.
    #[must_use]
    pub fn info(kind: NoticeKind, message: impl Into<String>) -> Self {
        Self::new(kind, Severity::Info, message)
    }

    /// A notice worth the user's attention.
    #[must_use]
    pub fn watch(kind: NoticeKind, message: impl Into<String>) -> Self {
        Self::new(kind, Severity::Watch, message)
    }

    /// A notice about something actively wrong.
    #[must_use]
    pub fn critical(kind: NoticeKind, message: impl Into<String>) -> Self {
        Self::new(kind, Severity::Critical, message)
    }

    /// The redundant, non-colour cue for this notice's severity (§5.2).
    #[must_use]
    pub const fn symbol(&self) -> char {
        self.severity.symbol()
    }

    /// The colour token that reinforces — never replaces — the symbol (§5.2).
    #[must_use]
    pub const fn token(&self) -> Token {
        match self.severity {
            Severity::Info => Token::Muted,
            Severity::Watch => Token::Watch,
            Severity::Critical => Token::Critical,
        }
    }

    /// Whether two notices say the same thing and should be merged.
    #[must_use]
    fn is_same_as(&self, other: &Self) -> bool {
        self.kind == other.kind && self.severity == other.severity && self.message == other.message
    }

    /// The rendered line: `! process PID 31842 has already exited`.
    #[must_use]
    pub fn render(&self) -> String {
        let repeat = if self.occurrences > 1 {
            format!(" (x{})", self.occurrences)
        } else {
            String::new()
        };
        format!(
            "{} {} {}{repeat}",
            self.symbol(),
            self.kind.label(),
            self.message
        )
    }
}

/// The bounded, de-duplicating notice list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NoticeLog {
    notices: Vec<Notice>,
    dropped: u64,
}

impl NoticeLog {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            notices: Vec::new(),
            dropped: 0,
        }
    }

    /// Records `notice` at monotonic time `now`.
    ///
    /// An identical notice already in the log has its count incremented and its
    /// timestamp refreshed, and stays where it is: a repeating failure must not
    /// scroll the interesting entries away.
    pub fn push(&mut self, mut notice: Notice, now: Instant) {
        notice.last_seen = Some(now);
        if let Some(existing) = self
            .notices
            .iter_mut()
            .find(|existing| existing.is_same_as(&notice))
        {
            existing.occurrences = existing.occurrences.saturating_add(1);
            existing.last_seen = Some(now);
            return;
        }
        if self.notices.len() >= MAX_NOTICES {
            self.notices.remove(0);
            self.dropped = self.dropped.saturating_add(1);
        }
        self.notices.push(notice);
    }

    /// Every retained notice, oldest first.
    #[must_use]
    pub fn as_slice(&self) -> &[Notice] {
        &self.notices
    }

    /// The most recent notice, which is the one the status line shows.
    #[must_use]
    pub fn latest(&self) -> Option<&Notice> {
        self.notices.last()
    }

    /// The most severe retained notice, for the header indicator.
    ///
    /// Ties go to the newest, so a second critical failure replaces the first in
    /// the one-line summary.
    #[must_use]
    pub fn most_severe(&self) -> Option<&Notice> {
        self.notices
            .iter()
            .rev()
            .max_by_key(|notice| notice.severity)
    }

    /// How many notices were evicted because the log was full.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether there is nothing to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.notices.is_empty()
    }

    /// How many notices are retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.notices.len()
    }

    /// Forgets everything. Used when the user dismisses the panel.
    pub fn clear(&mut self) -> bool {
        if self.notices.is_empty() {
            return false;
        }
        self.notices.clear();
        true
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_repeated_notice_is_counted_rather_than_duplicated() {
        let mut log = NoticeLog::new();
        let start = t0();
        for index in 0..50u32 {
            log.push(
                Notice::watch(NoticeKind::Collector, "/proc/diskstats read failed"),
                start + Duration::from_millis(u64::from(index)),
            );
        }

        assert_eq!(log.len(), 1);
        assert_eq!(log.as_slice().first().map(|n| n.occurrences), Some(50));
        assert!(
            log.as_slice()
                .first()
                .and_then(|n| n.last_seen)
                .is_some_and(|seen| seen > start),
            "the timestamp is refreshed by a repeat"
        );
    }

    #[test]
    fn the_log_is_bounded_and_keeps_the_newest_entries() {
        let mut log = NoticeLog::new();
        for index in 0..(MAX_NOTICES * 3) {
            log.push(
                Notice::info(NoticeKind::Interaction, format!("message {index}")),
                t0(),
            );
        }

        assert_eq!(log.len(), MAX_NOTICES);
        assert_eq!(
            log.latest().map(|n| n.message.as_str()),
            Some(format!("message {}", MAX_NOTICES * 3 - 1)).as_deref(),
            "a notice log exists to show the newest message"
        );
        assert_eq!(log.dropped(), (MAX_NOTICES * 2) as u64);
    }

    #[test]
    fn the_most_severe_notice_wins_the_header_regardless_of_order() {
        let mut log = NoticeLog::new();
        log.push(Notice::info(NoticeKind::Export, "wrote 4 KiB"), t0());
        log.push(
            Notice::critical(NoticeKind::Collector, "sampler thread stopped"),
            t0(),
        );
        log.push(
            Notice::info(NoticeKind::Interaction, "nothing selected"),
            t0(),
        );

        assert_eq!(
            log.most_severe().map(|n| n.severity),
            Some(Severity::Critical)
        );
        assert_eq!(log.latest().map(|n| n.kind), Some(NoticeKind::Interaction));
    }

    #[test]
    fn every_notice_carries_a_symbol_so_colour_is_never_the_only_cue() {
        for severity in [Severity::Info, Severity::Watch, Severity::Critical] {
            let notice = Notice::new(NoticeKind::Config, severity, "example");
            assert_eq!(notice.symbol(), severity.symbol());
            assert!(notice.render().starts_with(notice.symbol()));
        }
        assert_eq!(
            Notice::info(NoticeKind::Config, "example").token(),
            Token::Muted
        );
        assert_eq!(
            Notice::watch(NoticeKind::Config, "example").token(),
            Token::Watch
        );
        assert_eq!(
            Notice::critical(NoticeKind::Config, "example").token(),
            Token::Critical
        );
    }

    #[test]
    fn a_repeat_count_is_rendered_only_when_it_repeated() {
        let mut log = NoticeLog::new();
        log.push(Notice::info(NoticeKind::Collector, "slow sample"), t0());
        assert_eq!(
            log.latest().map(Notice::render),
            Some(". collector slow sample".to_owned())
        );
        log.push(Notice::info(NoticeKind::Collector, "slow sample"), t0());
        assert_eq!(
            log.latest().map(Notice::render),
            Some(". collector slow sample (x2)".to_owned())
        );
    }

    #[test]
    fn every_kind_has_a_distinct_label() {
        let mut labels: Vec<&str> = NoticeKind::ALL.iter().map(|kind| kind.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), NoticeKind::ALL.len());
    }

    #[test]
    fn clearing_reports_whether_anything_was_there() {
        let mut log = NoticeLog::new();
        assert!(!log.clear());
        log.push(Notice::info(NoticeKind::Terminal, "resized"), t0());
        assert!(log.clear());
        assert!(log.is_empty());
    }
}
