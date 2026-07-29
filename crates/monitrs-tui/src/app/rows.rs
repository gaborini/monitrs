//! The visible process rows: filter, then sort, then flatten (§7.2).
//!
//! The row list is *derived* state, rebuilt whenever the displayed snapshot, the
//! filter, the ordering or the flat/tree toggle changes. It exists for two
//! reasons:
//!
//! * **Selection has to be O(1) to move.** §3.1 requires input to stay responsive
//!   under load; re-sorting ten thousand processes on every `j` would not be.
//! * **The renderer must do no work that can fail.** A row carries the index of
//!   its process in the snapshot it was built from, so drawing a frame is a
//!   lookup rather than a search (§10.4 keeps the snapshot immutable, so the
//!   index stays valid for as long as the row list does).
//!
//! Rows never carry cloned process data. A published snapshot is shared behind an
//! `Arc` and a 10 000-row table must not be duplicated per tick (§16.1).

use monitrs_core::model::{ProcessIdentity, ProcessSnapshot, SystemSnapshot};
use monitrs_core::process::{ProcessFilter, ProcessSort, ProcessTree, flat_order};

/// Where a row sits in the process tree (§7.2 tree mode, §2.4 context).
///
/// Present only in tree mode. Flat rows carry `None` rather than a depth of zero:
/// "no hierarchy was computed" and "this row is a root" are different facts, and
/// a renderer that cannot tell them apart would draw tree branches on a flat list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeShape {
    /// Indentation level; `0` for a root.
    pub depth: u32,
    /// Row index of this row's parent, always smaller than this row's index.
    pub parent_row: Option<usize>,
    /// Total descendants, direct and indirect (§2.4).
    pub descendants: u32,
    /// Whether this row is the last of its sibling group, which selects `` `- ``
    /// over `+- ` when rendering.
    pub is_last_child: bool,
    /// Whether this row's parent link was cut to break a cycle in the reported
    /// parent graph, so the placement is our decision rather than the kernel's.
    pub parent_link_cut: bool,
}

/// One visible row of the process table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessRow {
    /// The stable identity of the process on this row (§26).
    pub identity: ProcessIdentity,
    /// Index into `SystemSnapshot::processes` of the snapshot these rows were
    /// built from.
    pub process_index: usize,
    /// Tree shape, in tree mode only.
    pub tree: Option<TreeShape>,
}

/// The visible rows, in display order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessRows {
    rows: Vec<ProcessRow>,
    tree_mode: bool,
    cycles_broken: u32,
}

impl ProcessRows {
    /// No rows at all, which is what an interface with no snapshot yet shows.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            rows: Vec::new(),
            tree_mode: false,
            cycles_broken: 0,
        }
    }

    /// Builds the visible rows of `snapshot`.
    ///
    /// Filtering happens before ordering in both modes. In tree mode a hidden
    /// process does not orphan its children — [`ProcessTree`] re-attaches them to
    /// the nearest surviving ancestor — so a filter reshapes the tree instead of
    /// scattering unrelated rows to the root.
    pub(in crate::app) fn build(
        snapshot: Option<&SystemSnapshot>,
        filter: &ProcessFilter,
        sort: ProcessSort,
        tree_mode: bool,
    ) -> Self {
        let Some(snapshot) = snapshot else {
            return Self {
                tree_mode,
                ..Self::empty()
            };
        };

        if tree_mode {
            let tree = ProcessTree::from_snapshot_filtered(snapshot, sort, filter);
            let rows = tree
                .rows()
                .iter()
                .map(|row| ProcessRow {
                    identity: row.identity,
                    process_index: row.process_index,
                    tree: Some(TreeShape {
                        depth: row.depth,
                        parent_row: row.parent_row,
                        descendants: row.descendants,
                        is_last_child: row.is_last_child,
                        parent_link_cut: row.parent_link_cut,
                    }),
                })
                .collect();
            return Self {
                rows,
                tree_mode,
                cycles_broken: tree.cycles_broken(),
            };
        }

        let rows = flat_order(&snapshot.processes, filter, sort)
            .into_iter()
            .filter_map(|index| {
                snapshot.processes.get(index).map(|process| ProcessRow {
                    identity: process.identity,
                    process_index: index,
                    tree: None,
                })
            })
            .collect();
        Self {
            rows,
            tree_mode,
            cycles_broken: 0,
        }
    }

    /// Every visible row, in display order.
    #[must_use]
    pub fn as_slice(&self) -> &[ProcessRow] {
        &self.rows
    }

    /// How many rows are visible.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether nothing is visible, which is a real state and not an error: a
    /// locked-down container can legitimately show no processes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The row at `index`, or `None` — never a panicking index (§18.2).
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&ProcessRow> {
        self.rows.get(index)
    }

    /// The last row's index, if there is one.
    #[must_use]
    pub fn last_index(&self) -> Option<usize> {
        self.rows.len().checked_sub(1)
    }

    /// Where `identity` currently is, if it is visible.
    ///
    /// Keyed on the full identity rather than the PID: a reused PID is a different
    /// process and must not inherit the selection (§26).
    #[must_use]
    pub fn row_of(&self, identity: ProcessIdentity) -> Option<usize> {
        self.rows.iter().position(|row| row.identity == identity)
    }

    /// Whether these rows were built in tree mode.
    #[must_use]
    pub const fn is_tree(&self) -> bool {
        self.tree_mode
    }

    /// How many parent links were cut to break cycles, for the Inspect screen.
    #[must_use]
    pub const fn cycles_broken(&self) -> u32 {
        self.cycles_broken
    }

    /// The process on `row` of `snapshot`.
    ///
    /// Validates the identity as well as the index: if `snapshot` is not the one
    /// these rows were built from, the answer is `None` rather than a different
    /// process's data (§26).
    #[must_use]
    pub fn process<'a>(
        &self,
        snapshot: &'a SystemSnapshot,
        row: usize,
    ) -> Option<&'a ProcessSnapshot> {
        let row = self.rows.get(row)?;
        let process = snapshot.processes.get(row.process_index)?;
        (process.identity == row.identity).then_some(process)
    }

    /// The visible processes in display order, for the search helpers in
    /// [`ProcessFilter`] which take rows in display order.
    #[must_use]
    pub fn processes<'a>(&self, snapshot: &'a SystemSnapshot) -> Vec<&'a ProcessSnapshot> {
        self.rows
            .iter()
            .filter_map(|row| {
                snapshot
                    .processes
                    .get(row.process_index)
                    .filter(|process| process.identity == row.identity)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::process::{ProcessSortKey, SortDirection};

    use super::*;
    use crate::app::fixtures::{Fake, snapshot_of};

    fn sort_by_cpu() -> ProcessSort {
        ProcessSort::new(ProcessSortKey::Cpu, SortDirection::Descending)
    }

    #[test]
    fn flat_rows_are_filtered_then_sorted_and_carry_no_tree_shape() {
        let snapshot = snapshot_of(
            0,
            &[
                Fake::new(1, 11, "launchd").cpu(0.1),
                Fake::new(2, 22, "rustc").cpu(287.0),
                Fake::new(3, 33, "rustc").cpu(54.0),
            ],
        );

        let rows = ProcessRows::build(
            Some(&snapshot),
            &ProcessFilter::parse("rustc"),
            sort_by_cpu(),
            false,
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.as_slice()
                .iter()
                .map(|row| row.identity.pid)
                .collect::<Vec<_>>(),
            vec![2, 3],
            "hottest first, launchd filtered out"
        );
        assert!(rows.as_slice().iter().all(|row| row.tree.is_none()));
        assert!(!rows.is_tree());
    }

    #[test]
    fn the_kernel_thread_toggle_removes_them_from_the_rows() {
        let snapshot = snapshot_of(
            1,
            &[
                Fake::new(1, 11, "launchd").cpu(0.1),
                Fake::new(42, 42, "kworker/2:1").cpu(0.4).kernel_thread(),
                Fake::new(2, 22, "rustc").cpu(287.0),
            ],
        );

        let all = ProcessRows::build(Some(&snapshot), &ProcessFilter::new(), sort_by_cpu(), false);
        assert_eq!(all.len(), 3);

        let hidden = ProcessRows::build(
            Some(&snapshot),
            &ProcessFilter::new().with_hidden_kernel_threads(true),
            sort_by_cpu(),
            false,
        );
        assert_eq!(hidden.len(), 2, "§7.2: kernel threads can be hidden");
        assert_eq!(hidden.row_of(ProcessIdentity::new(42, 42)), None);
    }

    #[test]
    fn ordering_by_memory_uses_resident_size() {
        let snapshot = snapshot_of(
            1,
            &[
                Fake::new(1, 11, "small").cpu(90.0).rss(8 * 1024 * 1024),
                Fake::new(2, 22, "large")
                    .cpu(1.0)
                    .rss(4 * 1024 * 1024 * 1024),
            ],
        );

        let rows = ProcessRows::build(
            Some(&snapshot),
            &ProcessFilter::new(),
            ProcessSort::new(ProcessSortKey::Memory, SortDirection::Descending),
            false,
        );

        assert_eq!(
            rows.as_slice()
                .iter()
                .map(|row| row.identity.pid)
                .collect::<Vec<_>>(),
            vec![2, 1],
            "the largest resident set first, whatever the CPU order would be"
        );
    }

    #[test]
    fn tree_rows_carry_a_shape_and_keep_children_under_parents() {
        let snapshot = snapshot_of(
            0,
            &[
                Fake::new(1, 11, "launchd").cpu(0.1),
                Fake::new(2, 22, "zsh").parent(1).cpu(1.0),
                Fake::new(3, 33, "cargo").parent(2).cpu(90.0),
            ],
        );

        let rows = ProcessRows::build(Some(&snapshot), &ProcessFilter::new(), sort_by_cpu(), true);

        assert!(rows.is_tree());
        let depths: Vec<u32> = rows
            .as_slice()
            .iter()
            .filter_map(|row| row.tree.map(|shape| shape.depth))
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
        assert_eq!(rows.cycles_broken(), 0);
    }

    #[test]
    fn a_row_resolves_to_its_process_only_in_the_snapshot_it_was_built_from() {
        let first = snapshot_of(0, &[Fake::new(7, 70, "node").cpu(3.0)]);
        let rows = ProcessRows::build(Some(&first), &ProcessFilter::new(), sort_by_cpu(), false);

        assert_eq!(
            rows.process(&first, 0).map(|process| &*process.name),
            Some("node")
        );

        // The same PID, a different process (§26).
        let recycled = snapshot_of(1, &[Fake::new(7, 99, "python").cpu(3.0)]);
        assert!(
            rows.process(&recycled, 0).is_none(),
            "a reused PID must not resolve through a stale row"
        );
        assert!(rows.process(&first, 9).is_none(), "out of range is None");
    }

    #[test]
    fn no_snapshot_means_no_rows_but_the_mode_is_remembered() {
        let rows = ProcessRows::build(None, &ProcessFilter::new(), sort_by_cpu(), true);
        assert!(rows.is_empty());
        assert!(rows.is_tree());
        assert_eq!(rows.last_index(), None);
        assert_eq!(rows.row_of(ProcessIdentity::new(1, 1)), None);
    }

    #[test]
    fn row_lookup_is_by_identity_not_by_pid() {
        let snapshot = snapshot_of(0, &[Fake::new(31_842, 900_100, "rustc").cpu(120.0)]);
        let rows = ProcessRows::build(Some(&snapshot), &ProcessFilter::new(), sort_by_cpu(), false);

        assert_eq!(rows.row_of(ProcessIdentity::new(31_842, 900_100)), Some(0));
        assert_eq!(rows.row_of(ProcessIdentity::new(31_842, 977_400)), None);
    }

    #[test]
    fn the_display_ordered_process_list_matches_the_rows() {
        let snapshot = snapshot_of(
            0,
            &[
                Fake::new(1, 11, "a").cpu(1.0),
                Fake::new(2, 22, "b").cpu(9.0),
            ],
        );
        let rows = ProcessRows::build(Some(&snapshot), &ProcessFilter::new(), sort_by_cpu(), false);

        let names: Vec<&str> = rows
            .processes(&snapshot)
            .into_iter()
            .map(|process| &*process.name)
            .collect();
        assert_eq!(names, vec!["b", "a"]);
    }
}
