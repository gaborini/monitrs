//! Process tree construction (§7.2 tree mode, §2.4 process context).
//!
//! A snapshot gives each process a bare `parent_pid` (§ the model deliberately
//! does not pretend to know the parent's start key), so the tree is resolved here
//! against the rest of the table. The result is a flat, pre-order list of
//! [`TreeRow`]s carrying depth, sibling position, descendant count, and parent
//! links — everything the renderer and the §2.4 breadcrumb need, with no borrowed
//! recursion for the UI to walk.
//!
//! # The three hostile cases (§17.1, §17.7)
//!
//! A process table is a *racing* read: every row was captured at a slightly
//! different moment, so the parent graph can be malformed in ways a well-behaved
//! kernel never produces.
//!
//! * **Missing parent.** A parent that exited between two reads is simply not in
//!   the table. Its children become roots. They are never dropped: a process the
//!   OS reported must appear on screen exactly once, or the table silently lies
//!   about what is running.
//! * **Self-parent.** `parent_pid == pid` is a 1-cycle. The link is cut and the
//!   row becomes a root.
//! * **Cycles.** A racing read (or a reused PID landing on an ancestor) can
//!   produce `a -> b -> a`. Nothing here recurses: construction is a bounded
//!   iteration over explicit stacks, so a cycle cannot overflow the stack. Each
//!   cycle has exactly one link cut, and the victim is the lowest
//!   [`ProcessIdentity`] in the cycle — a property of the cycle itself, not of the
//!   order the table was enumerated in, so two refreshes cut the same link and the
//!   tree does not reshuffle.
//!
//! Every one of those is *expected*, not an error (§14.1), which is why the only
//! trace they leave is [`TreeRow::parent_link_cut`] and
//! [`ProcessTree::cycles_broken`].

use core::cmp::Ordering;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::model::{AncestorEntry, ProcessIdentity, ProcessSnapshot, SystemSnapshot};
use crate::process::{ProcessFilter, ProcessSort};

/// One row of a rendered process tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TreeRow {
    /// The stable identity of the process on this row (§26).
    pub identity: ProcessIdentity,
    /// Index of the process in the slice the tree was built from.
    ///
    /// Rows are indices rather than clones because a published snapshot is shared
    /// behind an `Arc` and must not be duplicated per tick (§10.4, §16.1).
    pub process_index: usize,
    /// Indentation level: `0` for a root.
    ///
    /// `u32` rather than `u16` so a pathological chain cannot wrap; it saturates
    /// instead.
    pub depth: u32,
    /// Row index of this row's parent, always smaller than this row's own index.
    pub parent_row: Option<usize>,
    /// Total descendants, direct and indirect (§2.4).
    pub descendants: u32,
    /// Whether this row is the last of its sibling group.
    ///
    /// Selects `` `- `` over `+- ` when rendering, and for a depth-`0` row refers
    /// to the group of roots.
    pub is_last_child: bool,
    /// Whether this row's parent link was cut to break a cycle.
    ///
    /// A *missing* parent does not set this: it is ordinary and expected, whereas a
    /// cut link means the OS reported something impossible and the placement of
    /// this subtree is our decision rather than the kernel's.
    pub parent_link_cut: bool,
}

/// A process tree flattened into display order.
#[derive(Clone, Debug, Default)]
pub struct ProcessTree {
    rows: Vec<TreeRow>,
    cycles_broken: u32,
}

impl ProcessTree {
    /// Builds the tree of every process in `snapshot`.
    #[must_use]
    pub fn from_snapshot(snapshot: &SystemSnapshot, sort: ProcessSort) -> Self {
        Self::build(&snapshot.processes, sort)
    }

    /// Builds the tree of the processes in `snapshot` that pass `filter`.
    #[must_use]
    pub fn from_snapshot_filtered(
        snapshot: &SystemSnapshot,
        sort: ProcessSort,
        filter: &ProcessFilter,
    ) -> Self {
        Self::build_filtered(&snapshot.processes, sort, filter)
    }

    /// Builds the tree of every process in `processes`.
    #[must_use]
    pub fn build<P: Borrow<ProcessSnapshot>>(processes: &[P], sort: ProcessSort) -> Self {
        Self::build_filtered(processes, sort, &ProcessFilter::new())
    }

    /// Builds the tree of the processes that pass `filter`.
    ///
    /// A hidden process does not orphan its children: they re-attach to the
    /// nearest *surviving* ancestor, so hiding kernel threads or filtering by name
    /// reshapes the tree without scattering unrelated rows to the root. Rows that
    /// do not pass the filter simply do not appear.
    #[must_use]
    pub fn build_filtered<P: Borrow<ProcessSnapshot>>(
        processes: &[P],
        sort: ProcessSort,
        filter: &ProcessFilter,
    ) -> Self {
        let all: Vec<&ProcessSnapshot> = processes.iter().map(Borrow::borrow).collect();
        let count = all.len();
        if count == 0 {
            return Self::default();
        }
        let retained: Vec<bool> = all.iter().map(|process| filter.matches(process)).collect();

        let by_pid = index_by_pid(&all);
        let (mut parent, mut cut) = resolve_parents(&all, &by_pid);
        break_cycles(&all, &mut parent, &mut cut);
        let nearest = nearest_retained(&parent, &retained);

        let (roots, children) = group_children(&nearest, &retained, count);
        let rows = emit_rows(&all, &cut, sort, roots, children);
        let cycles_broken =
            u32::try_from(cut.iter().filter(|link| **link).count()).unwrap_or(u32::MAX);

        Self {
            rows,
            cycles_broken,
        }
    }

    /// Every row, in display order.
    #[must_use]
    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    /// How many rows the tree has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the tree has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The row at `index`, if there is one.
    #[must_use]
    pub fn row(&self, index: usize) -> Option<&TreeRow> {
        self.rows.get(index)
    }

    /// The row showing `identity`, if it is in the tree.
    ///
    /// Keyed on the full identity, so a reused PID resolves to nothing rather than
    /// to the wrong row (§26).
    #[must_use]
    pub fn row_of(&self, identity: ProcessIdentity) -> Option<usize> {
        self.rows.iter().position(|row| row.identity == identity)
    }

    /// How many parent links were cut to break a cycle, self-parents included.
    ///
    /// Zero on any sane system. A non-zero value is worth surfacing as collector
    /// health rather than hiding, because it means the process table was read
    /// while the graph was inconsistent.
    #[must_use]
    pub const fn cycles_broken(&self) -> u32 {
        self.cycles_broken
    }

    /// The deepest indentation level in the tree.
    #[must_use]
    pub fn max_depth(&self) -> u32 {
        self.rows.iter().map(|row| row.depth).max().unwrap_or(0)
    }

    /// The number of rows this row occupies together with its subtree.
    #[must_use]
    pub fn subtree_len(&self, index: usize) -> usize {
        self.rows
            .get(index)
            .map_or(0, |row| usize::try_from(row.descendants).unwrap_or(0) + 1)
    }

    /// The direct children of a row, in display order (§2.4 child navigation).
    #[must_use]
    pub fn child_rows(&self, index: usize) -> Vec<usize> {
        let Some(row) = self.rows.get(index) else {
            return Vec::new();
        };
        let child_depth = row.depth.saturating_add(1);
        let mut children = Vec::new();
        for (candidate_index, candidate) in self.rows.iter().enumerate().skip(index + 1) {
            if candidate.depth <= row.depth {
                break;
            }
            if candidate.depth == child_depth {
                children.push(candidate_index);
            }
        }
        children
    }

    /// The ancestors of a row, nearest parent first (§2.4 breadcrumb).
    ///
    /// Terminates because `parent_row` is always a smaller index than the row it
    /// belongs to: pre-order emission guarantees it, so the walk strictly
    /// decreases.
    #[must_use]
    pub fn ancestor_rows(&self, index: usize) -> Vec<usize> {
        let mut ancestors = Vec::new();
        let mut cursor = self.rows.get(index).and_then(|row| row.parent_row);
        while let Some(current) = cursor {
            ancestors.push(current);
            cursor = self.rows.get(current).and_then(|row| row.parent_row);
        }
        ancestors
    }

    /// Whether a vertical continuation line is needed at each level above a row.
    ///
    /// Element `i` covers indentation level `i` (root-most first) and is `true`
    /// when the ancestor at that level still has siblings below it, so the renderer
    /// draws `|  ` there and three spaces otherwise. The row's own connector comes
    /// from [`TreeRow::is_last_child`].
    #[must_use]
    pub fn continuation_flags(&self, index: usize) -> Vec<bool> {
        let mut flags: Vec<bool> = self
            .ancestor_rows(index)
            .into_iter()
            .filter_map(|ancestor| self.rows.get(ancestor))
            .map(|ancestor| !ancestor.is_last_child)
            .collect();
        flags.reverse();
        flags
    }

    /// The process a row refers to, revalidated against its identity.
    ///
    /// Returns `None` when `processes` is not the slice the tree was built from, so
    /// a stale tree paired with a fresh snapshot yields nothing instead of the
    /// wrong process (§26: a PID is not an identity).
    #[must_use]
    pub fn process<'a, P: Borrow<ProcessSnapshot>>(
        &self,
        processes: &'a [P],
        index: usize,
    ) -> Option<&'a ProcessSnapshot> {
        let row = self.rows.get(index)?;
        processes
            .get(row.process_index)
            .map(Borrow::borrow)
            .filter(|process| process.identity == row.identity)
    }

    /// The §2.4 ancestry breadcrumb for a row, nearest parent first.
    ///
    /// Matches the ordering of [`crate::model::ProcessDetail::ancestry`] so the
    /// live tree and an on-demand detail read render identically. Entries that
    /// cannot be revalidated against `processes` are skipped rather than guessed.
    #[must_use]
    pub fn ancestry<P: Borrow<ProcessSnapshot>>(
        &self,
        processes: &[P],
        index: usize,
    ) -> Vec<AncestorEntry> {
        self.ancestor_rows(index)
            .into_iter()
            .filter_map(|ancestor| self.process(processes, ancestor))
            .map(|process| AncestorEntry {
                identity: process.identity,
                name: process.name.clone(),
            })
            .collect()
    }
}

/// Maps each PID to the index of the process that owns it.
///
/// A snapshot should never contain two entries for one PID, but a racing read can
/// produce one. The lowest `start_key` wins, which is a property of the processes
/// rather than of enumeration order, so the choice is the same on every refresh.
fn index_by_pid(all: &[&ProcessSnapshot]) -> HashMap<u32, usize> {
    let mut by_pid: HashMap<u32, usize> = HashMap::with_capacity(all.len());
    for (index, process) in all.iter().enumerate() {
        match by_pid.entry(process.identity.pid) {
            Entry::Vacant(slot) => {
                slot.insert(index);
            }
            Entry::Occupied(mut slot) => {
                let incumbent = all.get(*slot.get()).map(|other| other.identity.start_key);
                if incumbent.is_some_and(|key| process.identity.start_key < key) {
                    slot.insert(index);
                }
            }
        }
    }
    by_pid
}

/// Resolves every `parent_pid` to an index, cutting self-parents.
///
/// Returns the parent of each process and which parent links were cut.
fn resolve_parents(
    all: &[&ProcessSnapshot],
    by_pid: &HashMap<u32, usize>,
) -> (Vec<Option<usize>>, Vec<bool>) {
    let mut parent: Vec<Option<usize>> = Vec::with_capacity(all.len());
    let mut cut: Vec<bool> = vec![false; all.len()];
    for (index, process) in all.iter().enumerate() {
        let resolved = process
            .parent_pid
            .and_then(|pid| by_pid.get(&pid).copied())
            .filter(|&candidate| candidate != index);
        if resolved.is_none()
            && process.parent_pid == Some(process.identity.pid)
            && let Some(link) = cut.get_mut(index)
        {
            *link = true;
        }
        parent.push(resolved);
    }
    (parent, cut)
}

/// Marks used while walking parent chains to find cycles.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Mark {
    /// Not yet visited by any walk.
    New,
    /// On the chain the current walk is following.
    OnChain,
    /// Known to lead to a root.
    Done,
}

/// Cuts one link in every cycle so every parent chain terminates.
///
/// Each process is pushed onto the chain at most once across all walks, so this is
/// linear in the number of processes and cannot recurse (§17.7).
fn break_cycles(all: &[&ProcessSnapshot], parent: &mut [Option<usize>], cut: &mut [bool]) {
    let count = parent.len();
    let mut marks: Vec<Mark> = vec![Mark::New; count];
    let mut chain: Vec<usize> = Vec::new();

    for start in 0..count {
        if marks.get(start).copied() != Some(Mark::New) {
            continue;
        }
        chain.clear();
        let mut cursor = start;
        loop {
            match marks.get(cursor).copied() {
                Some(Mark::New) => {
                    if let Some(mark) = marks.get_mut(cursor) {
                        *mark = Mark::OnChain;
                    }
                    chain.push(cursor);
                    match parent.get(cursor).copied().flatten() {
                        Some(next) => cursor = next,
                        None => break,
                    }
                }
                Some(Mark::OnChain) => {
                    // `cursor` closes a cycle with the suffix of the chain that
                    // starts at it. Cutting the lowest identity in that suffix is
                    // deterministic given the cycle, so refreshes agree.
                    let suffix_start = chain.iter().position(|&node| node == cursor);
                    let victim = suffix_start
                        .and_then(|position| chain.get(position..))
                        .and_then(|cycle| {
                            cycle
                                .iter()
                                .copied()
                                .min_by_key(|&node| all.get(node).map(|process| process.identity))
                        });
                    if let Some(victim) = victim {
                        if let Some(link) = parent.get_mut(victim) {
                            *link = None;
                        }
                        if let Some(flag) = cut.get_mut(victim) {
                            *flag = true;
                        }
                    }
                    break;
                }
                Some(Mark::Done) | None => break,
            }
        }
        for &node in &chain {
            if let Some(mark) = marks.get_mut(node) {
                *mark = Mark::Done;
            }
        }
    }
}

/// For each process, the nearest strict ancestor that survived the filter.
///
/// Memoized and iterative: each process joins the working chain at most once, so
/// this stays linear even when a long chain is filtered out entirely.
fn nearest_retained(parent: &[Option<usize>], retained: &[bool]) -> Vec<Option<usize>> {
    let count = parent.len();
    let mut nearest: Vec<Option<usize>> = vec![None; count];
    let mut resolved: Vec<bool> = vec![false; count];
    let mut chain: Vec<usize> = Vec::new();

    for start in 0..count {
        if resolved.get(start).copied() == Some(true) {
            continue;
        }
        chain.clear();
        let mut cursor = start;
        // Walk up to the first already-resolved ancestor, or to a root. Cycles are
        // already broken, so this terminates.
        let inherited = loop {
            chain.push(cursor);
            if chain.len() > count {
                // Unreachable while `break_cycles` holds: a chain longer than the
                // table must repeat a node. Bounding it anyway means a bug there
                // degrades to a flatter tree instead of hanging the collector.
                break None;
            }
            match parent.get(cursor).copied().flatten() {
                None => break None,
                Some(above) => {
                    if resolved.get(above).copied() == Some(true) {
                        break if retained.get(above).copied() == Some(true) {
                            Some(above)
                        } else {
                            nearest.get(above).copied().flatten()
                        };
                    }
                    cursor = above;
                }
            }
        };

        let mut accumulated = inherited;
        for &node in chain.iter().rev() {
            if let Some(slot) = nearest.get_mut(node) {
                *slot = accumulated;
            }
            if let Some(flag) = resolved.get_mut(node) {
                *flag = true;
            }
            if retained.get(node).copied() == Some(true) {
                accumulated = Some(node);
            }
        }
    }
    nearest
}

/// Splits the retained processes into roots and per-parent child lists.
fn group_children(
    nearest: &[Option<usize>],
    retained: &[bool],
    count: usize,
) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut roots: Vec<usize> = Vec::new();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (index, keep) in retained.iter().enumerate() {
        if !keep {
            continue;
        }
        match nearest.get(index).copied().flatten() {
            Some(parent) => {
                if let Some(list) = children.get_mut(parent) {
                    list.push(index);
                }
            }
            None => roots.push(index),
        }
    }
    (roots, children)
}

/// Emits the tree as a pre-order row list, sorting each sibling group.
///
/// Sorting sibling groups rather than the whole table is what keeps children under
/// their parents while still honouring the §7.2 sort: the busiest root comes first,
/// and within it the busiest child.
fn emit_rows(
    all: &[&ProcessSnapshot],
    cut: &[bool],
    sort: ProcessSort,
    mut roots: Vec<usize>,
    mut children: Vec<Vec<usize>>,
) -> Vec<TreeRow> {
    let compare = |left: &usize, right: &usize| -> Ordering {
        match (all.get(*left), all.get(*right)) {
            (Some(first), Some(second)) => sort.compare(first, second),
            // Unreachable: every index comes from `all`. Ordering by index keeps
            // the comparator total anyway.
            _ => left.cmp(right),
        }
    };
    roots.sort_by(compare);
    for group in &mut children {
        group.sort_by(compare);
    }

    let mut rows: Vec<TreeRow> = Vec::with_capacity(all.len());
    let mut stack: Vec<Frame> = Vec::new();
    push_group(&mut stack, &roots, 0, None);

    while let Some(frame) = stack.pop() {
        let Some(process) = all.get(frame.node) else {
            continue;
        };
        let row_index = rows.len();
        rows.push(TreeRow {
            identity: process.identity,
            process_index: frame.node,
            depth: frame.depth,
            parent_row: frame.parent_row,
            descendants: 0,
            is_last_child: frame.is_last,
            parent_link_cut: cut.get(frame.node).copied().unwrap_or(false),
        });
        if let Some(group) = children.get(frame.node) {
            push_group(
                &mut stack,
                group,
                frame.depth.saturating_add(1),
                Some(row_index),
            );
        }
    }

    fill_descendants(&mut rows);
    rows
}

/// One pending row, so emission is an explicit stack rather than recursion.
struct Frame {
    /// Index of the process in the table being walked.
    node: usize,
    /// Indentation level this row will be emitted at.
    depth: u32,
    /// Whether the row closes its sibling group.
    is_last: bool,
    /// Row index of the already-emitted parent.
    parent_row: Option<usize>,
}

/// Pushes a sibling group so that the first sibling is popped first.
fn push_group(stack: &mut Vec<Frame>, group: &[usize], depth: u32, parent_row: Option<usize>) {
    for (position, &node) in group.iter().enumerate().rev() {
        stack.push(Frame {
            node,
            depth,
            is_last: position + 1 == group.len(),
            parent_row,
        });
    }
}

/// Fills in [`TreeRow::descendants`] from the pre-order layout.
///
/// Walks backwards keeping the subtree sizes that have not yet found their parent.
/// Everything after a row with a greater depth is inside that row's subtree, which
/// makes this linear rather than a scan per row.
fn fill_descendants(rows: &mut [TreeRow]) {
    let mut pending: Vec<(u32, u32)> = Vec::new();
    for row in rows.iter_mut().rev() {
        let mut total: u32 = 0;
        while let Some(&(depth, size)) = pending.last() {
            if depth > row.depth {
                total = total.saturating_add(size);
                pending.pop();
            } else {
                break;
            }
        }
        row.descendants = total;
        pending.push((row.depth, total.saturating_add(1)));
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::super::fixtures::{process, snapshot};
    use super::*;
    use crate::process::{ProcessSortKey, SortDirection};

    fn pids(tree: &ProcessTree, processes: &[ProcessSnapshot]) -> Vec<u32> {
        tree.rows()
            .iter()
            .filter_map(|row| {
                processes
                    .get(row.process_index)
                    .map(|process| process.identity.pid)
            })
            .collect()
    }

    fn shape(tree: &ProcessTree, processes: &[ProcessSnapshot]) -> Vec<String> {
        tree.rows()
            .iter()
            .filter_map(|row| {
                processes.get(row.process_index).map(|process| {
                    let indent = "  ".repeat(usize::try_from(row.depth).unwrap_or(0));
                    format!("{indent}{}", process.name)
                })
            })
            .collect()
    }

    fn every_process_exactly_once(tree: &ProcessTree, count: usize) {
        assert_eq!(tree.len(), count, "row count must equal process count");
        let mut seen: Vec<usize> = tree.rows().iter().map(|row| row.process_index).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "every process must appear exactly once");
    }

    #[test]
    fn a_normal_tree_nests_children_under_their_parents() {
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(100, 2).name("sshd").parent(1).build(),
            process(200, 3).name("bash").parent(100).build(),
            process(300, 4).name("cron").parent(1).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        assert_eq!(
            shape(&tree, &processes),
            vec!["systemd", "  sshd", "    bash", "  cron"]
        );
        every_process_exactly_once(&tree, processes.len());
        assert_eq!(tree.cycles_broken(), 0);
        assert_eq!(tree.max_depth(), 2);
    }

    #[test]
    fn a_process_whose_parent_is_missing_becomes_a_root_and_is_never_dropped() {
        // PID 4242 exited between the two reads that produced this table.
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(500, 2).name("orphan").parent(4242).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, 2);
        assert_eq!(shape(&tree, &processes), vec!["systemd", "orphan"]);
        let orphan_row = tree.row_of(ProcessIdentity::new(500, 2)).expect("present");
        let orphan = tree.row(orphan_row).expect("present");
        assert_eq!(orphan.depth, 0);
        assert_eq!(orphan.parent_row, None);
        assert!(
            !orphan.parent_link_cut,
            "a missing parent is expected, not a broken graph"
        );
        assert_eq!(tree.cycles_broken(), 0);
    }

    #[test]
    fn a_self_parent_becomes_a_flagged_root() {
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(7, 2).name("ouroboros").parent(7).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, 2);
        let row_index = tree.row_of(ProcessIdentity::new(7, 2)).expect("present");
        let row = tree.row(row_index).expect("present");
        assert_eq!(row.depth, 0);
        assert!(row.parent_link_cut);
        assert_eq!(tree.cycles_broken(), 1);
    }

    #[test]
    fn a_two_cycle_is_broken_deterministically_and_keeps_both_rows() {
        let processes = vec![
            process(10, 5).name("a").parent(20).build(),
            process(20, 6).name("b").parent(10).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, 2);
        assert_eq!(tree.cycles_broken(), 1);
        // The lowest identity in the cycle is PID 10, so it is the one promoted.
        assert_eq!(shape(&tree, &processes), vec!["a", "  b"]);

        // The same table enumerated the other way round must cut the same link.
        let reversed: Vec<ProcessSnapshot> = processes.iter().rev().cloned().collect();
        let other = ProcessTree::build(&reversed, ProcessSort::ascending(ProcessSortKey::Pid));
        assert_eq!(shape(&other, &reversed), vec!["a", "  b"]);
    }

    #[test]
    fn a_three_cycle_is_broken_and_keeps_every_row() {
        let processes = vec![
            process(30, 1).name("c").parent(20).build(),
            process(20, 1).name("b").parent(10).build(),
            process(10, 1).name("a").parent(30).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, 3);
        assert_eq!(tree.cycles_broken(), 1);
        assert_eq!(shape(&tree, &processes), vec!["a", "  b", "    c"]);
        assert_eq!(tree.max_depth(), 2);
    }

    #[test]
    fn a_cycle_with_an_outside_subtree_attached_keeps_everything() {
        let processes = vec![
            process(10, 1).name("a").parent(20).build(),
            process(20, 1).name("b").parent(10).build(),
            process(30, 1).name("child-of-b").parent(20).build(),
            process(40, 1).name("grandchild").parent(30).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, 4);
        assert_eq!(
            shape(&tree, &processes),
            vec!["a", "  b", "    child-of-b", "      grandchild"]
        );
    }

    #[test]
    fn two_independent_cycles_are_both_broken() {
        let processes = vec![
            process(10, 1).name("a").parent(11).build(),
            process(11, 1).name("b").parent(10).build(),
            process(20, 1).name("c").parent(21).build(),
            process(21, 1).name("d").parent(20).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, 4);
        assert_eq!(tree.cycles_broken(), 2);
    }

    #[test]
    fn a_ten_thousand_deep_chain_does_not_overflow_the_stack() {
        // §17.1/§17.7: construction must be iterative. A recursive builder dies
        // here, and so does a recursive descendant count.
        let depth: u32 = 10_000;
        let processes: Vec<ProcessSnapshot> = (1..=depth)
            .map(|pid| {
                let fixture = process(pid, u64::from(pid));
                if pid == 1 {
                    fixture.build()
                } else {
                    fixture.parent(pid - 1).build()
                }
            })
            .collect();
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, processes.len());
        assert_eq!(tree.max_depth(), depth - 1);
        let root = tree.row(0).expect("a root");
        assert_eq!(root.depth, 0);
        assert_eq!(root.descendants, depth - 1);
        assert_eq!(tree.subtree_len(0), processes.len());
        assert_eq!(tree.ancestor_rows(tree.len() - 1).len(), 9_999);
    }

    #[test]
    fn a_ten_thousand_long_cycle_terminates() {
        let length: u32 = 10_000;
        let processes: Vec<ProcessSnapshot> = (1..=length)
            .map(|pid| {
                let parent = if pid == 1 { length } else { pid - 1 };
                process(pid, u64::from(pid)).parent(parent).build()
            })
            .collect();
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, processes.len());
        assert_eq!(tree.cycles_broken(), 1);
    }

    #[test]
    fn descendants_count_direct_and_indirect_children() {
        let processes = vec![
            process(1, 1).name("root").build(),
            process(2, 1).name("a").parent(1).build(),
            process(3, 1).name("a1").parent(2).build(),
            process(4, 1).name("a2").parent(2).build(),
            process(5, 1).name("b").parent(1).build(),
            process(6, 1).name("lonely").build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        let descendants = |pid: u32, start_key: u64| {
            let index = tree
                .row_of(ProcessIdentity::new(pid, start_key))
                .expect("present");
            tree.row(index).expect("present").descendants
        };
        assert_eq!(descendants(1, 1), 4);
        assert_eq!(descendants(2, 1), 2);
        assert_eq!(descendants(3, 1), 0);
        assert_eq!(descendants(5, 1), 0);
        assert_eq!(descendants(6, 1), 0);
    }

    #[test]
    fn sorting_reorders_siblings_without_moving_them_out_of_their_parent() {
        let processes = vec![
            process(1, 1).name("root").cpu(1.0).build(),
            process(2, 1).name("quiet").parent(1).cpu(1.0).build(),
            process(3, 1).name("busy").parent(1).cpu(90.0).build(),
            process(4, 1).name("busy-child").parent(2).cpu(99.0).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::default());
        assert_eq!(
            shape(&tree, &processes),
            vec!["root", "  busy", "  quiet", "    busy-child"],
            "the hottest process stays under its parent"
        );
    }

    #[test]
    fn sibling_order_is_stable_when_their_values_are_equal() {
        let build = |order: [u32; 3]| {
            let mut processes = vec![process(1, 1).name("root").build()];
            for pid in order {
                processes.push(
                    process(pid, u64::from(pid))
                        .name(&format!("child{pid}"))
                        .parent(1)
                        .cpu(0.0)
                        .build(),
                );
            }
            let tree = ProcessTree::build(&processes, ProcessSort::default());
            shape(&tree, &processes)
        };
        assert_eq!(build([2, 3, 4]), build([4, 3, 2]));
        assert_eq!(
            build([3, 2, 4]),
            vec!["root", "  child2", "  child3", "  child4"]
        );
    }

    #[test]
    fn roots_are_sorted_too() {
        let processes = vec![
            process(1, 1).name("quiet").cpu(1.0).build(),
            process(2, 2).name("busy").cpu(80.0).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::default());
        assert_eq!(shape(&tree, &processes), vec!["busy", "quiet"]);
        assert!(!tree.row(0).expect("present").is_last_child);
        assert!(
            tree.row(1).expect("present").is_last_child,
            "the final root closes the top-level group"
        );
    }

    #[test]
    fn filtered_rows_disappear_and_their_children_join_the_nearest_survivor() {
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(2, 1)
                .name("kthreadd")
                .parent(1)
                .kernel_thread()
                .build(),
            process(3, 1)
                .name("kworker/0")
                .parent(2)
                .kernel_thread()
                .build(),
            process(4, 1).name("app").parent(2).build(),
        ];
        let filter = ProcessFilter::new().with_hidden_kernel_threads(true);
        let tree = ProcessTree::build_filtered(
            &processes,
            ProcessSort::ascending(ProcessSortKey::Pid),
            &filter,
        );
        assert_eq!(
            shape(&tree, &processes),
            vec!["systemd", "  app"],
            "app re-attaches to systemd rather than becoming a root"
        );
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn a_filtered_out_root_leaves_its_children_as_roots() {
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(2, 1).name("app").parent(1).build(),
        ];
        let filter = ProcessFilter::parse("app");
        let tree = ProcessTree::build_filtered(&processes, ProcessSort::default(), &filter);
        assert_eq!(shape(&tree, &processes), vec!["app"]);
        assert_eq!(tree.row(0).expect("present").parent_row, None);
    }

    #[test]
    fn a_filter_that_hides_a_long_chain_still_terminates() {
        let length: u32 = 5_000;
        let mut processes: Vec<ProcessSnapshot> = (1..=length)
            .map(|pid| {
                let fixture = process(pid, u64::from(pid)).name("hidden");
                if pid == 1 {
                    fixture.build()
                } else {
                    fixture.parent(pid - 1).build()
                }
            })
            .collect();
        processes.push(process(90_001, 1).name("visible").parent(length).build());
        let tree = ProcessTree::build_filtered(
            &processes,
            ProcessSort::default(),
            &ProcessFilter::parse("visible"),
        );
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.row(0).expect("present").depth, 0);
    }

    #[test]
    fn duplicate_pids_from_a_racing_read_do_not_lose_rows() {
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(50, 10).name("old").parent(1).build(),
            process(50, 99).name("new").parent(1).build(),
            process(60, 1).name("child-of-50").parent(50).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        every_process_exactly_once(&tree, 4);
        // The lower start key owns the PID, so the child attaches to `old`.
        assert_eq!(
            shape(&tree, &processes),
            vec!["systemd", "  old", "    child-of-50", "  new"]
        );
    }

    #[test]
    fn child_and_ancestor_navigation_walks_the_structure() {
        let processes = vec![
            process(1, 1).name("root").build(),
            process(2, 1).name("a").parent(1).build(),
            process(3, 1).name("a1").parent(2).build(),
            process(4, 1).name("b").parent(1).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        assert_eq!(tree.child_rows(0), vec![1, 3]);
        assert_eq!(tree.child_rows(1), vec![2]);
        assert_eq!(tree.child_rows(2), Vec::<usize>::new());
        assert_eq!(tree.ancestor_rows(2), vec![1, 0]);
        assert_eq!(tree.ancestor_rows(0), Vec::<usize>::new());
        assert_eq!(tree.child_rows(99), Vec::<usize>::new());
        assert_eq!(tree.ancestor_rows(99), Vec::<usize>::new());
        assert_eq!(tree.subtree_len(99), 0);
    }

    #[test]
    fn continuation_flags_describe_the_vertical_lines_of_the_ascii_shape() {
        // systemd
        // +- sshd
        // |  `- bash
        // `- cron
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(2, 1).name("sshd").parent(1).build(),
            process(3, 1).name("bash").parent(2).build(),
            process(4, 1).name("cron").parent(1).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        assert_eq!(tree.continuation_flags(0), Vec::<bool>::new());
        assert_eq!(tree.continuation_flags(1), vec![false]);
        assert_eq!(
            tree.continuation_flags(2),
            vec![false, true],
            "sshd has a sibling below it, so bash needs a vertical bar"
        );
        assert!(!tree.row(1).expect("present").is_last_child);
        assert!(tree.row(3).expect("present").is_last_child);
    }

    #[test]
    fn the_breadcrumb_lists_ancestors_nearest_first() {
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(2, 1).name("sshd").parent(1).build(),
            process(3, 1).name("bash").parent(2).build(),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        let row = tree.row_of(ProcessIdentity::new(3, 1)).expect("present");
        let ancestry = tree.ancestry(&processes, row);
        assert_eq!(
            ancestry
                .iter()
                .map(|entry| entry.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["sshd", "systemd"]
        );
        assert_eq!(
            ancestry.first().map(|entry| entry.identity),
            Some(ProcessIdentity::new(2, 1))
        );
    }

    #[test]
    fn a_row_resolved_against_the_wrong_snapshot_yields_nothing() {
        let processes = vec![process(1, 1).name("systemd").build()];
        let tree = ProcessTree::build(&processes, ProcessSort::default());
        // The next snapshot has PID 1 belonging to a different process.
        let recycled = vec![process(1, 99).name("impostor").build()];
        assert!(tree.process(&recycled, 0).is_none());
        assert!(tree.ancestry(&recycled, 0).is_empty());
        assert!(tree.process(&processes, 0).is_some());
        assert!(tree.process(&processes, 7).is_none());
    }

    #[test]
    fn an_empty_table_produces_an_empty_tree() {
        let processes: Vec<ProcessSnapshot> = Vec::new();
        let tree = ProcessTree::build(&processes, ProcessSort::default());
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.max_depth(), 0);
        assert_eq!(tree.cycles_broken(), 0);
        assert!(tree.row(0).is_none());
        assert!(tree.row_of(ProcessIdentity::new(1, 1)).is_none());
        assert!(tree.continuation_flags(0).is_empty());
    }

    #[test]
    fn a_tree_can_be_built_straight_from_a_snapshot() {
        let live = snapshot(vec![
            process(1, 1).name("systemd").build(),
            process(2, 1).name("app").parent(1).build(),
        ]);
        let tree = ProcessTree::from_snapshot(&live, ProcessSort::default());
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.row(1).expect("present").depth, 1);

        let filtered = ProcessTree::from_snapshot_filtered(
            &live,
            ProcessSort::default(),
            &ProcessFilter::parse("app"),
        );
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn a_tree_can_be_built_from_borrowed_rows() {
        let processes = vec![
            process(1, 1).name("systemd").build(),
            process(2, 1).name("app").parent(1).build(),
        ];
        let borrowed: Vec<&ProcessSnapshot> = processes.iter().collect();
        let tree = ProcessTree::build(&borrowed, ProcessSort::default());
        assert_eq!(pids(&tree, &processes), vec![1, 2]);
    }

    proptest! {
        /// §17.7: "arbitrary process graphs do not cause recursion overflow or
        /// cycles". The generator deliberately produces self-parents, cycles,
        /// missing parents, and forests.
        #[test]
        fn arbitrary_parent_graphs_terminate_and_keep_every_process(
            parents in prop::collection::vec(0u32..40, 1..40),
            descending in any::<bool>(),
        ) {
            let count = u32::try_from(parents.len()).unwrap_or(u32::MAX);
            let processes: Vec<ProcessSnapshot> = parents
                .iter()
                .enumerate()
                .map(|(index, &raw)| {
                    let pid = u32::try_from(index).unwrap_or(0) + 1;
                    let fixture = process(pid, u64::from(pid));
                    // `0` and anything above `count` name a process that is not in
                    // the table; everything else is a real row, including this one.
                    if raw == 0 {
                        fixture.build()
                    } else {
                        fixture.parent(raw).build()
                    }
                })
                .collect();

            let direction = SortDirection::from_descending(descending);
            let tree = ProcessTree::build(
                &processes,
                ProcessSort::new(ProcessSortKey::Pid, direction),
            );

            prop_assert_eq!(tree.len(), processes.len());
            let mut seen: Vec<usize> = tree.rows().iter().map(|row| row.process_index).collect();
            seen.sort_unstable();
            seen.dedup();
            prop_assert_eq!(seen.len(), processes.len(), "a process was dropped or duplicated");

            let mut total_roots = 0u32;
            for (row_index, row) in tree.rows().iter().enumerate() {
                // Pre-order: a parent is always emitted before its children, which
                // is what makes ancestor walks terminate.
                if let Some(parent_row) = row.parent_row {
                    prop_assert!(parent_row < row_index);
                    let parent = tree.row(parent_row).expect("parent row exists");
                    prop_assert_eq!(parent.depth + 1, row.depth);
                } else {
                    prop_assert_eq!(row.depth, 0);
                    total_roots += 1;
                }
                prop_assert!(row.depth < count);
                prop_assert!(tree.ancestor_rows(row_index).len() == usize::try_from(row.depth).unwrap_or(0));
            }
            prop_assert!(total_roots >= 1, "a cyclic forest still needs a root");

            // Every row is inside exactly one root subtree.
            let root_subtrees: usize = tree
                .rows()
                .iter()
                .enumerate()
                .filter(|(_, row)| row.parent_row.is_none())
                .map(|(index, _)| tree.subtree_len(index))
                .sum();
            prop_assert_eq!(root_subtrees, processes.len());
        }
    }
}
