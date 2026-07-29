//! Time Lens state: `LIVE`, `PAUSED`, and `HISTORY -MM:SS` (§2.1).
//!
//! [`monitrs_core::history::HistoryView`] owns the cursor arithmetic over the
//! ring. This type owns the three things it deliberately does not model:
//!
//! * **Pausing is not seeking.** §2.1 lists `PAUSED` as a header state of its own.
//!   A user who presses `Space` has frozen the *visible* timeline without asking
//!   for a specific earlier sample, and telling them they are 12 seconds in the
//!   past would misdescribe what they did — even though, by then, they are.
//! * **Freezing is not stopping.** Collection continues while the display is
//!   frozen (§2.1), so the app keeps two snapshots: the newest one received and
//!   the one being shown. This type decides which of those the renderer sees.
//! * **Not live means no process actions.** §2.1 and §15.1 disable process
//!   control while historical data is displayed, and a paused view *is* historical
//!   data the moment the next sample lands: the PID on screen may already have
//!   exited. [`Timeline::allows_process_actions`] is therefore stricter than
//!   [`HistoryView::allows_process_actions`], which only knows about the cursor.

use core::time::Duration;

use monitrs_core::history::{HistoricalSample, HistoryRing, HistoryView, SeekOutcome};
use monitrs_core::units::format_age;

use crate::action::Seek;
use crate::theme::Token;

/// What the header must show (§2.1).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TimelineStatus {
    /// Following the newest sample.
    Live,
    /// Frozen by `Space`, without a specific sample selected.
    Paused {
        /// How far behind live the frozen view has drifted since it was paused.
        ///
        /// Not part of the header word: it is offered so a renderer with room can
        /// add it, and so the status line can explain why the numbers stopped
        /// moving.
        behind: Duration,
    },
    /// A specific earlier sample is selected.
    History {
        /// How far behind live that sample is.
        offset: Duration,
    },
}

impl TimelineStatus {
    /// The header word, in exactly the three forms §2.1 specifies.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Live => "LIVE".to_owned(),
            Self::Paused { .. } => "PAUSED".to_owned(),
            // Not `format_history_offset`: it renders a zero offset as `LIVE`,
            // which inside a `HISTORY` badge would be a contradiction. A cursor
            // parked on the newest sample is still history — §2.1 makes returning
            // to live an explicit action, so it must not *look* live.
            Self::History { offset } => format!("HISTORY -{}", format_age(offset)),
        }
    }

    /// The redundant, non-colour cue (§5.2).
    ///
    /// `>` follows, `=` is frozen, `<` looks backwards. All three are 7-bit ASCII
    /// because §5.1 requires the whole interface to work in strict-ASCII mode.
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Live => '>',
            Self::Paused { .. } => '=',
            Self::History { .. } => '<',
        }
    }

    /// The colour token that reinforces the symbol.
    ///
    /// `Accent` for history on purpose: §26 requires historical and live state to
    /// be *visually unmistakable*, and the accent is the one colour a screen uses
    /// for the thing the user must not miss. `Stale` for paused says exactly what
    /// a paused view is — a retained value that is no longer fresh.
    #[must_use]
    pub const fn token(self) -> Token {
        match self {
            Self::Live => Token::Good,
            Self::Paused { .. } => Token::Stale,
            Self::History { .. } => Token::Accent,
        }
    }

    /// Whether this state shows live data.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }

    /// Whether the header must read as *not now* (§26).
    #[must_use]
    pub const fn is_frozen(self) -> bool {
        !self.is_live()
    }

    /// How far behind live the displayed state is, whichever way it got there.
    #[must_use]
    pub const fn behind(self) -> Duration {
        match self {
            Self::Live => Duration::ZERO,
            Self::Paused { behind } => behind,
            Self::History { offset } => offset,
        }
    }
}

/// The Time Lens cursor plus the pause flag (§2.1).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timeline {
    view: HistoryView,
    paused: bool,
    scrubbed: bool,
    last_seek: Option<SeekOutcome>,
}

impl Timeline {
    /// A timeline following live samples.
    #[must_use]
    pub const fn live() -> Self {
        Self {
            view: HistoryView::live(),
            paused: false,
            scrubbed: false,
            last_seek: None,
        }
    }

    /// The cursor, for the panels that read the ring themselves.
    #[must_use]
    pub const fn view(self) -> HistoryView {
        self.view
    }

    /// Whether the displayed state is the newest sample.
    #[must_use]
    pub const fn is_live(self) -> bool {
        !self.paused && !self.scrubbed
    }

    /// Whether `Space` froze the display.
    #[must_use]
    pub const fn is_paused(self) -> bool {
        self.paused
    }

    /// Whether a specific earlier sample has been selected.
    #[must_use]
    pub const fn is_historical(self) -> bool {
        self.scrubbed
    }

    /// Whether process-control actions may be offered (§2.1, §15.1).
    #[must_use]
    pub const fn allows_process_actions(self) -> bool {
        self.is_live()
    }

    /// What the most recent seek did, so the UI can show that history ran out
    /// instead of appearing to ignore the key.
    #[must_use]
    pub const fn last_seek(self) -> Option<SeekOutcome> {
        self.last_seek
    }

    /// The header state, resolved against the ring for the offset.
    #[must_use]
    pub fn status(self, ring: &HistoryRing) -> TimelineStatus {
        let behind = self.view.offset_from_live(ring);
        if self.scrubbed {
            TimelineStatus::History { offset: behind }
        } else if self.paused {
            TimelineStatus::Paused { behind }
        } else {
            TimelineStatus::Live
        }
    }

    /// The selected sample, or the newest one when live.
    #[must_use]
    pub fn selected_sample(self, ring: &HistoryRing) -> Option<&HistoricalSample> {
        self.view.selected(ring)
    }

    /// Freezes the visible timeline, pinning the newest recorded sample.
    ///
    /// Pinning rather than leaving the cursor on `Live` is what makes a paused
    /// view *stay* on the sample the user was looking at: a `Live` cursor resolves
    /// to whatever is newest, which would keep the attribution panel moving under
    /// a header that says `PAUSED`.
    pub(in crate::app) fn pause(&mut self, ring: &HistoryRing) -> bool {
        if self.paused {
            return false;
        }
        self.paused = true;
        // A zero-distance step selects the sample the cursor already resolves to
        // without moving it, which is the only way to pin a `HistoryView` — its
        // position field is private and its API is movement. On an empty ring it
        // reports `Empty` and leaves the cursor following live, which is correct:
        // there is no sample to pin.
        let _ = self.view.step_back(ring, 0);
        // `scrubbed` stays false: the user froze the display, they did not ask for
        // an earlier sample, so the header reads PAUSED rather than HISTORY.
        true
    }

    /// Returns to live: the one explicit action §2.1 specifies for `L`.
    pub(in crate::app) fn return_live(&mut self) -> bool {
        if self.is_live() && self.view.is_live() {
            return false;
        }
        self.view.return_live();
        self.paused = false;
        self.scrubbed = false;
        self.last_seek = None;
        true
    }

    /// `Space`: freezes a live timeline, resumes a frozen one (§2.1, §5.6).
    ///
    /// Resuming from history is a return to live rather than a return to the
    /// paused-at-the-newest-sample state, because §5.6 labels the key *resume
    /// display* and a display that resumes into the past has not resumed.
    pub(in crate::app) fn toggle_pause(&mut self, ring: &HistoryRing) -> bool {
        if self.is_live() {
            self.pause(ring)
        } else {
            self.return_live()
        }
    }

    /// Moves the cursor, entering history if it was live.
    ///
    /// Returns [`SeekOutcome::Empty`] and changes nothing when the ring holds no
    /// samples: there is nothing to select, and pretending to enter history would
    /// disable process actions for no reason.
    pub(in crate::app) fn seek(&mut self, ring: &HistoryRing, seek: Seek) -> SeekOutcome {
        if seek.is_noop() {
            return SeekOutcome::Moved;
        }
        let outcome = match seek {
            Seek::Backward(steps) => self.view.step_back(ring, as_steps(steps)),
            Seek::Forward(steps) => self.view.step_forward(ring, as_steps(steps)),
            Seek::Oldest => self.view.step_back(ring, usize::MAX),
            Seek::Newest => self.view.step_forward(ring, usize::MAX),
        };
        if matches!(outcome, SeekOutcome::Empty) {
            return outcome;
        }
        self.scrubbed = true;
        self.last_seek = Some(outcome);
        outcome
    }
}

/// Converts a seek distance, saturating rather than wrapping.
fn as_steps(steps: u32) -> usize {
    usize::try_from(steps).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Instant, SystemTime};

    use monitrs_core::history::{HistoryConfig, HistoryRing};
    use monitrs_core::model::SystemSnapshot;

    use super::*;

    /// A ring with `count` recorded samples, one nominal second apart.
    fn ring_with(count: u64) -> HistoryRing {
        let start = Instant::now();
        let mut ring = HistoryRing::with_config(HistoryConfig::default(), start);
        for index in 0..count {
            let mut snapshot = SystemSnapshot::warming_up(
                start + Duration::from_secs(index),
                SystemTime::UNIX_EPOCH,
                8,
            );
            snapshot.sequence = index;
            snapshot.elapsed = Duration::from_secs(1);
            let _ = ring.record(&snapshot);
        }
        ring
    }

    #[test]
    fn a_fresh_timeline_is_live_and_allows_process_actions() {
        let timeline = Timeline::live();
        assert!(timeline.is_live());
        assert!(timeline.allows_process_actions());
        assert_eq!(timeline.status(&ring_with(3)), TimelineStatus::Live);
        assert_eq!(TimelineStatus::Live.label(), "LIVE");
    }

    #[test]
    fn pausing_reads_as_paused_and_forbids_process_actions() {
        let ring = ring_with(5);
        let mut timeline = Timeline::live();

        assert!(timeline.pause(&ring));
        assert!(!timeline.is_live());
        assert!(timeline.is_paused());
        assert!(!timeline.is_historical());
        assert!(
            !timeline.allows_process_actions(),
            "§15.1: a frozen view is historical data"
        );
        assert_eq!(timeline.status(&ring).label(), "PAUSED");
        assert!(!timeline.pause(&ring), "pausing twice changes nothing");
    }

    #[test]
    fn a_paused_view_keeps_its_sample_while_collection_continues() {
        let ring = ring_with(5);
        let mut timeline = Timeline::live();
        assert!(timeline.pause(&ring));
        let pinned = timeline
            .selected_sample(&ring)
            .map(|sample| sample.sequence);
        assert_eq!(pinned, Some(4));

        let later = ring_with(9);
        assert_eq!(
            timeline.selected_sample(&later).map(|s| s.sequence),
            Some(4),
            "the pinned sample must not drift as new samples arrive (§2.1)"
        );
        assert!(
            timeline.status(&later).behind() > Duration::ZERO,
            "the paused view has fallen behind live"
        );
        assert_eq!(timeline.status(&later).label(), "PAUSED");
    }

    #[test]
    fn seeking_switches_the_header_to_history_with_an_offset() {
        let ring = ring_with(10);
        let mut timeline = Timeline::live();

        assert_eq!(timeline.seek(&ring, Seek::step_back()), SeekOutcome::Moved);

        assert!(timeline.is_historical());
        assert!(!timeline.is_live());
        assert!(!timeline.allows_process_actions());
        assert_eq!(timeline.status(&ring).label(), "HISTORY -00:01");
        assert_eq!(timeline.status(&ring).symbol(), '<');
        assert_eq!(timeline.status(&ring).token(), Token::Accent);
    }

    #[test]
    fn a_cursor_parked_on_the_newest_sample_still_reads_as_history() {
        // §2.1: returning to live is one explicit action, so stepping forward to
        // the newest sample must not look live.
        let ring = ring_with(4);
        let mut timeline = Timeline::live();
        assert_eq!(timeline.seek(&ring, Seek::step_back()), SeekOutcome::Moved);
        assert_eq!(
            timeline.seek(&ring, Seek::step_forward()),
            SeekOutcome::Moved
        );

        assert!(timeline.is_historical());
        assert_eq!(timeline.status(&ring).label(), "HISTORY -00:00");
    }

    #[test]
    fn seeking_past_the_ends_clamps_and_says_so() {
        let ring = ring_with(6);
        let mut timeline = Timeline::live();

        assert_eq!(
            timeline.seek(&ring, Seek::Oldest),
            SeekOutcome::ClampedAtOldest
        );
        assert_eq!(timeline.last_seek(), Some(SeekOutcome::ClampedAtOldest));
        assert_eq!(
            timeline.seek(&ring, Seek::Newest),
            SeekOutcome::ClampedAtNewest
        );
        assert!(
            timeline.is_historical(),
            "clamping forward is not going live"
        );
    }

    #[test]
    fn seeking_an_empty_ring_changes_nothing() {
        let ring = ring_with(0);
        let mut timeline = Timeline::live();

        assert_eq!(timeline.seek(&ring, Seek::step_back()), SeekOutcome::Empty);

        assert!(timeline.is_live(), "there is no history to enter");
        assert_eq!(timeline.last_seek(), None);
    }

    #[test]
    fn returning_live_clears_both_pause_and_scrub() {
        let ring = ring_with(8);
        let mut timeline = Timeline::live();
        assert!(timeline.pause(&ring));
        let _ = timeline.seek(&ring, Seek::Backward(3));

        assert!(timeline.return_live());

        assert!(timeline.is_live());
        assert!(!timeline.is_paused());
        assert!(!timeline.is_historical());
        assert_eq!(timeline.last_seek(), None);
        assert!(timeline.allows_process_actions());
        assert!(
            !timeline.return_live(),
            "returning live twice changes nothing"
        );
    }

    #[test]
    fn space_resumes_from_history_rather_than_returning_to_a_paused_state() {
        let ring = ring_with(8);
        let mut timeline = Timeline::live();
        let _ = timeline.seek(&ring, Seek::Backward(2));

        assert!(timeline.toggle_pause(&ring));

        assert!(timeline.is_live(), "§5.6 labels Space `resume display`");
        assert_eq!(timeline.status(&ring), TimelineStatus::Live);
    }

    #[test]
    fn space_toggles_between_live_and_paused() {
        let ring = ring_with(3);
        let mut timeline = Timeline::live();
        assert!(timeline.toggle_pause(&ring));
        assert!(timeline.is_paused());
        assert!(timeline.toggle_pause(&ring));
        assert!(timeline.is_live());
    }

    #[test]
    fn every_status_carries_a_distinct_ascii_symbol_and_token() {
        let states = [
            TimelineStatus::Live,
            TimelineStatus::Paused {
                behind: Duration::from_secs(3),
            },
            TimelineStatus::History {
                offset: Duration::from_secs(37),
            },
        ];
        let mut symbols: Vec<char> = states.iter().map(|state| state.symbol()).collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), states.len());
        for state in states {
            assert!(state.symbol().is_ascii());
            assert!(state.label().is_ascii());
        }
        assert_eq!(
            TimelineStatus::History {
                offset: Duration::from_secs(37)
            }
            .label(),
            "HISTORY -00:37",
            "§2.1's exact header form"
        );
        assert!(TimelineStatus::Live.is_live());
        assert!(
            TimelineStatus::Paused {
                behind: Duration::ZERO
            }
            .is_frozen()
        );
    }

    #[test]
    fn a_zero_distance_seek_is_a_no_op() {
        let ring = ring_with(4);
        let mut timeline = Timeline::live();
        assert_eq!(timeline.seek(&ring, Seek::Backward(0)), SeekOutcome::Moved);
        assert!(timeline.is_live(), "a no-op seek must not enter history");
    }
}
