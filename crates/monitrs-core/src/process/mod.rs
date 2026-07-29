//! Process list logic: ordering, filtering, and tree construction.
//!
//! This module is the *list* half of the process view. The row data itself lives
//! in [`crate::model::ProcessSnapshot`]; nothing here mutates a snapshot, because
//! §10.4 requires published snapshots to stay immutable. Every entry point
//! therefore returns indices into the slice it was given, in display order.
//!
//! # What §7.2 demands of this module
//!
//! * **Stable sorting with a PID/start-time tie-breaker.** Every comparison ends
//!   in a [`crate::model::ProcessIdentity`] comparison, so rows with equal keys
//!   keep the same relative order on every refresh and selection does not jump.
//! * **Unavailable is not zero** (§26). A metric that was never measured sorts to
//!   the end of the list in *both* directions instead of pretending to be `0`.
//! * **Plain text filtering** over name, command, PID, and user, with the
//!   user-only and hide-kernel-threads toggles expressed as separate composable
//!   predicates.
//! * **Tree mode** that preserves parent-child relationships, keeps children
//!   under their parents when sorted, and survives a malformed parent graph.
//!
//! # Rendering a tree row
//!
//! [`ProcessTree`] emits a flat pre-order list of [`TreeRow`]s carrying a depth,
//! a last-sibling flag, and (via [`ProcessTree::continuation_flags`]) whether an
//! ancestor still has siblings below it. That is everything a renderer needs for
//! the strict-ASCII shape §5.1 requires:
//!
//! ```text
//! systemd
//! +- sshd
//! |  `- bash
//! `- cron
//! ```
//!
//! # Example
//!
//! ```
//! use monitrs_core::process::{ProcessFilter, ProcessSort, flat_order};
//!
//! # fn view(processes: &[monitrs_core::model::ProcessSnapshot]) -> Vec<usize> {
//! let filter = ProcessFilter::parse("rustc");
//! flat_order(processes, &filter, ProcessSort::default())
//! # }
//! ```

use std::borrow::Borrow;

use crate::model::ProcessSnapshot;

mod filter;
mod sort;
mod tree;

#[cfg(test)]
mod fixtures;

pub use filter::{FilterPattern, PlainPattern, ProcessFilter, ProcessPredicate};
pub use sort::{ProcessSort, ProcessSortKey, SortDirection, UnknownSortKey};
pub use tree::{ProcessTree, TreeRow};

/// The visible rows of the flat process view, in display order.
///
/// Filters first and sorts second, and returns indices into `processes` rather
/// than clones: the snapshot is shared behind an `Arc` and must not be copied per
/// tick (§10.4, §16.1).
///
/// Accepts both `&[ProcessSnapshot]` and `&[&ProcessSnapshot]` so a caller that
/// already narrowed the list does not have to clone rows to sort them.
#[must_use]
pub fn flat_order<P: Borrow<ProcessSnapshot>>(
    processes: &[P],
    filter: &ProcessFilter,
    sort: ProcessSort,
) -> Vec<usize> {
    let mut rows: Vec<(usize, &ProcessSnapshot)> = processes
        .iter()
        .map(Borrow::borrow)
        .enumerate()
        .filter(|(_, process)| filter.matches(process))
        .collect();
    rows.sort_by(|(_, left), (_, right)| sort.compare(left, right));
    rows.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::fixtures::process;
    use super::*;
    use crate::model::ProcessState;

    #[test]
    fn the_flat_view_filters_before_it_sorts() {
        let processes = vec![
            process(1, 1).name("systemd").cpu(1.0).build(),
            process(2, 2).name("rustc").cpu(50.0).build(),
            process(3, 3).name("rustc").cpu(90.0).build(),
        ];
        let order = flat_order(
            &processes,
            &ProcessFilter::parse("rustc"),
            ProcessSort::default(),
        );
        assert_eq!(order, vec![2, 1], "only rustc rows, hottest first");
    }

    #[test]
    fn an_inactive_filter_keeps_every_row() {
        let processes = vec![
            process(1, 1).cpu(1.0).build(),
            process(2, 2).cpu(2.0).build(),
        ];
        let order = flat_order(&processes, &ProcessFilter::new(), ProcessSort::default());
        assert_eq!(order.len(), processes.len());
    }

    #[test]
    fn the_flat_view_accepts_borrowed_rows_without_cloning_them() {
        let processes = [
            process(7, 7).cpu(5.0).build(),
            process(8, 8).cpu(9.0).build(),
        ];
        let borrowed: Vec<&ProcessSnapshot> = processes.iter().collect();
        let order = flat_order(&borrowed, &ProcessFilter::new(), ProcessSort::default());
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn composed_toggles_and_text_all_apply_to_the_flat_view() {
        let processes = vec![
            process(1, 1)
                .name("kworker/0:1")
                .user(0, Some("root"))
                .kernel_thread()
                .build(),
            process(2, 2).name("cargo").user(501, Some("gabor")).build(),
            process(3, 3).name("cargo").user(0, Some("root")).build(),
            process(4, 4)
                .name("zsh")
                .user(501, Some("gabor"))
                .state(ProcessState::Sleeping)
                .build(),
        ];
        let filter = ProcessFilter::parse("cargo")
            .with_only_user(Some(501))
            .with_hidden_kernel_threads(true);
        let order = flat_order(&processes, &filter, ProcessSort::default());
        assert_eq!(order, vec![1]);
    }
}
