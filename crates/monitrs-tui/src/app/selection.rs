//! Stable selection: the cursor that must not jump (§7.2).
//!
//! §7.2 is explicit: *do not allow row selection to jump unpredictably on each
//! refresh; track selection by stable process identity and preserve visual
//! position where possible.* Those two clauses are in tension whenever the table
//! is re-sorted, so the order they are applied in is the whole design:
//!
//! 1. **Identity wins.** If the selected [`ProcessIdentity`] is still visible, it
//!    stays selected, wherever it moved to. Re-sorting, filtering and a new
//!    snapshot all preserve the selection this way — the user is watching a
//!    process, not a screen row.
//! 2. **Position is the fallback.** If the selected process is gone — it exited,
//!    or a filter now hides it — the remembered row index is reused, clamped into
//!    the new list. That row is the nearest surviving neighbour of the one that
//!    disappeared, which is what "preserve visual position" means and is why the
//!    cursor does not snap back to the top of a 10 000-row table.
//! 3. **A reused PID inherits nothing.** Identity comparison includes the start
//!    key, so a recycled PID is simply not the selected process (§26).
//!
//! Movement clamps rather than wraps. A `j` at the bottom of the list reports "no
//! change" so the reducer can skip the redraw (§16.1 forbids a redraw busy loop),
//! and a table that jumped from the last row to the first would be exactly the
//! unpredictable movement §7.2 rules out.

use monitrs_core::model::ProcessIdentity;

use super::rows::ProcessRows;

/// What re-synchronising the selection against a new row list did.
///
/// Returned rather than swallowed so tests can assert *which* rule applied, and
/// so the reducer can tell "the process I was watching exited" from "the table
/// was re-sorted".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resync {
    /// There is nothing to select.
    Empty,
    /// The same process is still selected, possibly on a different row.
    Retained {
        /// Where it is now.
        row: usize,
    },
    /// The selected process is no longer visible; the nearest surviving row was
    /// selected instead of resetting to the top (§7.2).
    Replaced {
        /// What was selected before.
        lost: ProcessIdentity,
        /// The row now selected.
        row: usize,
    },
    /// Nothing was selected before — the first snapshot, or an empty table that
    /// filled up — so the first row was selected.
    Initialised {
        /// The row now selected.
        row: usize,
    },
}

impl Resync {
    /// The row now selected, if any.
    #[must_use]
    pub const fn row(self) -> Option<usize> {
        match self {
            Self::Empty => None,
            Self::Retained { row } | Self::Replaced { row, .. } | Self::Initialised { row } => {
                Some(row)
            }
        }
    }

    /// Whether the selected process survived.
    #[must_use]
    pub const fn kept_the_same_process(self) -> bool {
        matches!(self, Self::Retained { .. })
    }
}

/// The process-table cursor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    identity: Option<ProcessIdentity>,
    /// The last row the selection occupied.
    ///
    /// Kept even when `identity` is `None` so that a table which briefly empties
    /// and refills does not throw the user back to the top.
    row: usize,
    /// Whether the user has ever chosen what is selected.
    ///
    /// Rule 1 above — identity wins — is about a process *the user is watching*.
    /// Until they have pointed at one, there is nothing to watch, and following
    /// the identity that happened to be on row 0 is actively harmful: the first
    /// snapshot cannot order by CPU at all (§8.2 makes every rate `WarmingUp`),
    /// so row 0 of that table is an arbitrary process, and the viewport then
    /// follows it to wherever the real ordering puts it. On a busy machine that
    /// leaves a fresh session looking at row 78 of 989 with the hottest
    /// processes scrolled off the top.
    chosen: bool,
}

impl Selection {
    /// Nothing selected yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            identity: None,
            row: 0,
            chosen: false,
        }
    }

    /// The selected process, keyed by stable identity (§26).
    #[must_use]
    pub const fn identity(&self) -> Option<ProcessIdentity> {
        self.identity
    }

    /// The selected row, or `None` when nothing is selected.
    #[must_use]
    pub const fn row(&self) -> Option<usize> {
        match self.identity {
            Some(_) => Some(self.row),
            None => None,
        }
    }

    /// The remembered row, whether or not anything is selected.
    ///
    /// This is the value the fallback rule uses, and the reason a vanished
    /// selection lands next to where it was.
    #[must_use]
    pub const fn remembered_row(&self) -> usize {
        self.row
    }

    /// Re-synchronises against a freshly built row list.
    ///
    /// Call after every rebuild — new snapshot, new filter, new ordering, tree
    /// toggle — and never anywhere else: this is the single place the §7.2 rules
    /// live.
    pub(in crate::app) fn resync(&mut self, rows: &ProcessRows) -> Resync {
        let Some(last) = rows.last_index() else {
            // Keep `row` so a table that refills lands where it left off.
            self.identity = None;
            return Resync::Empty;
        };

        if !self.chosen {
            // An automatic selection tracks the top of the table, not the process
            // that was there when it was made. See `chosen`.
            self.row = 0;
            self.identity = rows.get(0).map(|row| row.identity);
            return Resync::Initialised { row: 0 };
        }

        if let Some(identity) = self.identity {
            if let Some(row) = rows.row_of(identity) {
                self.row = row;
                return Resync::Retained { row };
            }
            let row = self.row.min(last);
            self.row = row;
            self.identity = rows.get(row).map(|row| row.identity);
            return Resync::Replaced {
                lost: identity,
                row,
            };
        }

        let row = self.row.min(last);
        self.row = row;
        self.identity = rows.get(row).map(|row| row.identity);
        Resync::Initialised { row }
    }

    /// Selects `index`, clamped into `rows`. Reports whether anything changed.
    ///
    /// Every user-initiated selection funnels through here — movement, `gg`, `G`,
    /// selecting by identity, a click — which is what makes this the one place
    /// that can mark the selection as *chosen*. Note that it marks it even when
    /// the row does not change: pressing `j` at the bottom of the table is still
    /// the user saying which process they are watching.
    pub(in crate::app) fn select_row(&mut self, rows: &ProcessRows, index: usize) -> bool {
        let Some(last) = rows.last_index() else {
            return false;
        };
        let target = index.min(last);
        let identity = rows.get(target).map(|row| row.identity);
        self.chosen = true;
        if self.row == target && self.identity == identity {
            return false;
        }
        self.row = target;
        self.identity = identity;
        true
    }

    /// Selects `identity` if it is visible. Reports whether anything changed.
    pub(in crate::app) fn select_identity(
        &mut self,
        rows: &ProcessRows,
        identity: ProcessIdentity,
    ) -> bool {
        match rows.row_of(identity) {
            Some(row) => self.select_row(rows, row),
            None => false,
        }
    }

    /// Marks what is selected as the user's own choice.
    ///
    /// Acting on a row — opening its detail, pinning it — is choosing it just as
    /// much as moving the cursor onto it is: after such an action the cursor has
    /// to stay with *that* process instead of drifting back to the top of the
    /// table under it.
    pub(in crate::app) const fn confirm(&mut self) {
        self.chosen = true;
    }

    /// Moves `delta` rows, clamping at both ends.
    pub(in crate::app) fn step(&mut self, rows: &ProcessRows, delta: i64) -> bool {
        if rows.is_empty() {
            return false;
        }
        let current = i64::try_from(self.row).unwrap_or(i64::MAX);
        let target = current.saturating_add(delta).max(0);
        let target = usize::try_from(target).unwrap_or(usize::MAX);
        self.select_row(rows, target)
    }

    /// Selects the first row (`gg`, `Home`).
    pub(in crate::app) fn first(&mut self, rows: &ProcessRows) -> bool {
        self.select_row(rows, 0)
    }

    /// Selects the last row (`G`, `End`).
    pub(in crate::app) fn last(&mut self, rows: &ProcessRows) -> bool {
        match rows.last_index() {
            Some(last) => self.select_row(rows, last),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::process::{ProcessFilter, ProcessSort, ProcessSortKey, SortDirection};

    use super::*;
    use crate::app::fixtures::{Fake, snapshot_of};

    fn cpu_sort() -> ProcessSort {
        ProcessSort::new(ProcessSortKey::Cpu, SortDirection::Descending)
    }

    fn rows_of(sequence: u64, processes: &[Fake]) -> ProcessRows {
        let snapshot = snapshot_of(sequence, processes);
        ProcessRows::build(Some(&snapshot), &ProcessFilter::new(), cpu_sort(), false)
    }

    fn table() -> [Fake; 4] {
        [
            Fake::new(1, 11, "launchd").cpu(0.1),
            Fake::new(2, 22, "rustc").cpu(287.0),
            Fake::new(3, 33, "postgres").cpu(54.0),
            Fake::new(4, 44, "node").cpu(12.0),
        ]
    }

    #[test]
    fn the_first_row_is_selected_when_a_table_first_appears() {
        let mut selection = Selection::new();
        assert_eq!(selection.identity(), None);
        assert_eq!(selection.row(), None);

        let rows = rows_of(1, &table());
        assert_eq!(selection.resync(&rows), Resync::Initialised { row: 0 });
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(2, 22)));
    }

    /// The bug this pins was found by rendering a frame from the live collector,
    /// not by a unit test: every rule below behaved as designed, and the result
    /// was still that a fresh session opened on `launchd` at row 78 of 989 with
    /// the busiest processes scrolled off the top.
    ///
    /// The mechanism: §8.2 makes every rate `WarmingUp` on the first snapshot, so
    /// the CPU sort has nothing to order by and the unavailable-last rule falls
    /// through to the `(pid, start_key)` tie-break — row 0 is PID 1. Latching the
    /// selection onto that identity then dragged the viewport after it for the
    /// rest of the session, because the viewport follows the selection.
    #[test]
    fn an_automatic_selection_follows_the_top_row_until_the_user_chooses_one() {
        // A first snapshot exactly as §8.2 requires it: no rate is measurable yet.
        let warming = [
            Fake::new(1, 11, "launchd"),
            Fake::new(2, 22, "rustc"),
            Fake::new(3, 33, "postgres"),
        ];
        let mut selection = Selection::new();
        let rows = rows_of(1, &warming);
        assert_eq!(selection.resync(&rows), Resync::Initialised { row: 0 });
        assert_eq!(
            selection.identity(),
            Some(ProcessIdentity::new(1, 11)),
            "with no CPU values to sort by, row 0 is the lowest PID"
        );

        // The rates arrive and the table takes its real shape.
        let rows = rows_of(2, &table());
        assert_eq!(selection.resync(&rows), Resync::Initialised { row: 0 });
        assert_eq!(
            selection.identity(),
            Some(ProcessIdentity::new(2, 22)),
            "an unchosen cursor must be on the busiest process, not on whatever \
             happened to sort first before any rate existed"
        );
        assert_eq!(selection.row(), Some(0), "so the table opens at its top");

        // One deliberate keypress, and §7.2 rule 1 takes over for good.
        assert!(selection.select_identity(&rows, ProcessIdentity::new(4, 44)));
        let quieter = [
            Fake::new(1, 11, "launchd").cpu(0.1),
            Fake::new(2, 22, "rustc").cpu(287.0),
            Fake::new(3, 33, "postgres").cpu(54.0),
            Fake::new(4, 44, "node").cpu(0.2),
        ];
        let rows = rows_of(3, &quieter);
        assert_eq!(
            selection.resync(&rows),
            Resync::Retained { row: 2 },
            "node fell to third by CPU and the cursor went with it"
        );
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(4, 44)));
    }

    #[test]
    fn acting_on_a_row_counts_as_choosing_it() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(2, 22)));

        // `Enter`, `p` and the signal dialog all confirm without moving.
        selection.confirm();

        let reordered = [
            Fake::new(1, 11, "launchd").cpu(500.0),
            Fake::new(2, 22, "rustc").cpu(1.0),
            Fake::new(3, 33, "postgres").cpu(54.0),
            Fake::new(4, 44, "node").cpu(12.0),
        ];
        let rows = rows_of(2, &reordered);
        assert_eq!(
            selection.resync(&rows),
            Resync::Retained { row: 3 },
            "the process a dialog is about must not slide out from under it"
        );
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(2, 22)));
    }

    #[test]
    fn a_new_snapshot_keeps_the_selected_process_even_when_its_row_moves() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);
        assert!(selection.select_identity(&rows, ProcessIdentity::new(4, 44)));
        assert_eq!(selection.row(), Some(2), "node is third by CPU");

        // node becomes the busiest process; the table re-sorts under the cursor.
        let hotter = [
            Fake::new(1, 11, "launchd").cpu(0.1),
            Fake::new(2, 22, "rustc").cpu(20.0),
            Fake::new(3, 33, "postgres").cpu(10.0),
            Fake::new(4, 44, "node").cpu(300.0),
        ];
        let rows = rows_of(2, &hotter);

        assert_eq!(selection.resync(&rows), Resync::Retained { row: 0 });
        assert_eq!(
            selection.identity(),
            Some(ProcessIdentity::new(4, 44)),
            "§7.2: selection follows the process, not the row"
        );
    }

    #[test]
    fn re_sorting_does_not_move_the_selection_to_another_process() {
        let snapshot = snapshot_of(1, &table());
        let descending =
            ProcessRows::build(Some(&snapshot), &ProcessFilter::new(), cpu_sort(), false);
        let mut selection = Selection::new();
        let _ = selection.resync(&descending);
        assert!(selection.select_identity(&descending, ProcessIdentity::new(3, 33)));

        let ascending = ProcessRows::build(
            Some(&snapshot),
            &ProcessFilter::new(),
            ProcessSort::new(ProcessSortKey::Cpu, SortDirection::Ascending),
            false,
        );
        assert_eq!(selection.resync(&ascending), Resync::Retained { row: 2 });
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(3, 33)));
    }

    #[test]
    fn an_exited_process_hands_the_selection_to_its_nearest_surviving_neighbour() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);
        assert!(selection.select_identity(&rows, ProcessIdentity::new(3, 33)));
        assert_eq!(selection.row(), Some(1), "postgres is second by CPU");

        // postgres exits at sequence 2.
        let after = [
            Fake::new(1, 11, "launchd").cpu(0.1),
            Fake::new(2, 22, "rustc").cpu(287.0),
            Fake::new(3, 33, "postgres").cpu(54.0).exiting_at(2),
            Fake::new(4, 44, "node").cpu(12.0),
        ];
        let rows = rows_of(2, &after);

        assert_eq!(
            selection.resync(&rows),
            Resync::Replaced {
                lost: ProcessIdentity::new(3, 33),
                row: 1,
            },
            "the row postgres occupied is now node's; that is the nearest survivor"
        );
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(4, 44)));
        assert_ne!(selection.row(), Some(0), "§7.2: never reset to the top");
    }

    #[test]
    fn a_reused_pid_does_not_inherit_the_selection() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);
        assert_eq!(
            selection.identity(),
            Some(ProcessIdentity::new(2, 22)),
            "rustc is the busiest process, so it starts selected"
        );
        // §26's rule is about the process the user is *watching*, so the cursor has
        // to be theirs before the reuse question means anything.
        selection.confirm();

        let recycled = [
            Fake::new(1, 11, "launchd").cpu(0.1),
            Fake::new(2, 22, "rustc")
                .cpu(287.0)
                .exiting_at(2)
                .reused_as(999),
            Fake::new(3, 33, "postgres").cpu(54.0),
            Fake::new(4, 44, "node").cpu(12.0),
        ];
        let rows = rows_of(2, &recycled);

        let resync = selection.resync(&rows);
        assert!(
            matches!(resync, Resync::Replaced { lost, .. } if lost == ProcessIdentity::new(2, 22)),
            "got {resync:?}"
        );
        assert_ne!(
            selection.identity(),
            Some(ProcessIdentity::new(2, 999)),
            "the identity that took the PID is a different process (§26)"
        );
    }

    #[test]
    fn the_last_row_selection_survives_a_shrinking_table() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);
        assert!(selection.last(&rows));
        assert_eq!(selection.row(), Some(3), "launchd is last by CPU");

        // The two lowest-CPU rows are gone, so the remembered row 3 no longer
        // exists and the selected process is not in the new table either.
        let two = [
            Fake::new(2, 22, "rustc").cpu(287.0),
            Fake::new(3, 33, "postgres").cpu(54.0),
        ];
        let rows = rows_of(2, &two);

        let resync = selection.resync(&rows);
        assert_eq!(resync.row(), Some(1), "clamped to the new last row");
        assert!(!resync.kept_the_same_process());
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(3, 33)));
    }

    #[test]
    fn an_empty_table_clears_the_selection_but_remembers_the_row() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);
        assert!(selection.select_row(&rows, 2));

        let empty = rows_of(2, &[]);
        assert_eq!(selection.resync(&empty), Resync::Empty);
        assert_eq!(selection.identity(), None);
        assert_eq!(selection.row(), None);
        assert_eq!(selection.remembered_row(), 2);

        // Refilling puts the cursor back where it was, not at the top.
        let rows = rows_of(3, &table());
        assert_eq!(selection.resync(&rows), Resync::Initialised { row: 2 });
    }

    #[test]
    fn movement_clamps_at_both_ends_and_reports_no_change() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);

        assert!(!selection.step(&rows, -1), "already at the first row");
        assert!(selection.step(&rows, 1));
        assert_eq!(selection.row(), Some(1));
        assert!(selection.step(&rows, 100));
        assert_eq!(selection.row(), Some(3), "clamped, not wrapped");
        assert!(!selection.step(&rows, 5));
        assert!(selection.first(&rows));
        assert_eq!(selection.row(), Some(0));
        assert!(!selection.first(&rows));
    }

    #[test]
    fn movement_on_an_empty_table_is_a_no_op() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &[]);
        assert!(!selection.step(&rows, 1));
        assert!(!selection.first(&rows));
        assert!(!selection.last(&rows));
        assert!(!selection.select_row(&rows, 0));
        assert!(!selection.select_identity(&rows, ProcessIdentity::new(1, 1)));
    }

    #[test]
    fn selecting_an_invisible_process_changes_nothing() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        let _ = selection.resync(&rows);
        assert!(!selection.select_identity(&rows, ProcessIdentity::new(404, 404)));
        assert_eq!(selection.row(), Some(0));
    }

    #[test]
    fn repeated_resyncs_are_idempotent() {
        let mut selection = Selection::new();
        let rows = rows_of(1, &table());
        // An unchosen cursor re-derives itself from the same rows and lands in the
        // same place; it reports `Initialised` each time because that is what it
        // did, and the reducer discards the report anyway.
        assert_eq!(selection.resync(&rows), Resync::Initialised { row: 0 });
        assert_eq!(selection.resync(&rows), Resync::Initialised { row: 0 });
        assert_eq!(selection.identity(), Some(ProcessIdentity::new(2, 22)));

        selection.confirm();
        assert_eq!(selection.resync(&rows), Resync::Retained { row: 0 });
        assert_eq!(selection.resync(&rows), Resync::Retained { row: 0 });
    }
}
