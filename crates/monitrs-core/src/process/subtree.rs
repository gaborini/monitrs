//! Summing a process and its descendants (§2.4).
//!
//! A build, a container's entrypoint, a browser: the thing a user cares about is often
//! not one process but a family of them, and no single row answers "how much is this
//! costing me". This module answers it, and the whole difficulty is in being honest
//! about the answer.
//!
//! # A sum over metrics that may not exist
//!
//! Twenty-three processes, three of which will not report their CPU because they belong
//! to another user. What is the total?
//!
//! Reporting the sum of the twenty that did answer, as though it were the sum of
//! twenty-three, is the failure §4 exists to prevent — it is a number that looks
//! complete and is not. Reporting the whole thing as [`MetricState::PermissionDenied`]
//! because one member refused is literally true and useless: the user wanted to know
//! roughly what their build costs, and "unavailable" tells them nothing they could act
//! on.
//!
//! So a sum carries its **coverage**: how many members contributed to it, out of how
//! many there are. That is the same device §2.2 already uses for spike attribution —
//! "78% of observed CPU accounted for by the retained top contributors" — and the same
//! reasoning: a partial answer with its own limits stated is worth more than either a
//! confident fiction or a refusal.
//!
//! A sum with *no* contributors is not zero. It is whatever the members said, collapsed:
//! all-denied becomes [`MetricState::PermissionDenied`], all-warming-up becomes
//! [`MetricState::WarmingUp`]. Zero is reserved for "measured, and it was zero".
//!
//! # What a subtree is, and when it stops being one
//!
//! Membership is by [`ProcessIdentity`], never by PID, so a recycled PID joins nothing
//! (§26). The root is included in its own subtree: "this build" means the `cargo`
//! process *and* the compilers it spawned.
//!
//! When the root exits, this returns [`None`]. It does not follow the surviving
//! children: the kernel reparents them to init, and a set of processes whose common
//! ancestor is gone is not the family the user asked to watch. Calling it one would be
//! a fiction of exactly the kind this crate refuses elsewhere.
//!
//! # Cycles
//!
//! `/proc` can be read mid-`fork` and produce a parent chain that loops. The walk is
//! bounded by the number of processes in the snapshot and each is visited once, so a
//! cycle cannot make it hang — the same guarantee [`ProcessTree`] gives, arrived at the
//! same way.
//!
//! [`ProcessTree`]: crate::process::ProcessTree

use std::collections::{HashMap, HashSet};

use crate::model::{MetricState, ProcessIdentity, ProcessSnapshot, SystemSnapshot};
use crate::units::{Percent, Rate};

/// How much of a sum's membership actually contributed to it.
///
/// `contributors` is never greater than `members`; a coverage where they are equal is a
/// complete sum, and one where `contributors` is zero means nothing could be read at
/// all — in which case the accompanying [`MetricState`] is unavailable rather than zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Coverage {
    /// Members whose value was readable and went into the sum.
    pub contributors: usize,
    /// Members the subtree holds.
    pub members: usize,
}

impl Coverage {
    /// Whether every member contributed, making the sum exact.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.contributors >= self.members
    }

    /// Members that could not be read.
    #[must_use]
    pub const fn missing(&self) -> usize {
        self.members.saturating_sub(self.contributors)
    }

    /// The share of members that contributed, for display.
    ///
    /// `None` for an empty membership rather than 100%: a share of nothing is not
    /// complete, it is undefined, and §4 forbids inventing the difference.
    #[must_use]
    pub fn share(&self) -> Option<Percent> {
        if self.members == 0 {
            return None;
        }
        // Narrowing to f32 for a figure displayed with one decimal.
        #[allow(clippy::cast_precision_loss)]
        let share = (self.contributors as f32 / self.members as f32) * 100.0;
        Percent::new(share)
    }
}

/// One summed metric and the coverage of the sum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summed<T> {
    /// The sum of every member that could be read, or why none could be.
    pub value: MetricState<T>,
    /// How much of the membership the sum accounts for.
    pub coverage: Coverage,
}

impl<T> Summed<T> {
    /// Whether there is a number here that **understates** the subtree.
    ///
    /// This is the question a renderer actually has to answer, and it is not the same as
    /// "is the coverage complete". A metric the platform does not report at all has no
    /// contributors and therefore incomplete coverage — but there is no sum to
    /// understate, and marking it partial would suggest a permission problem where the
    /// truth is that per-process I/O does not exist on this OS. The [`MetricState`]
    /// already says which of those it is; this says only "the figure you can see is
    /// smaller than the truth".
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.value.fresh().is_some() && !self.coverage.is_complete()
    }
}

/// What a process and its descendants are using, together.
#[derive(Clone, Debug, PartialEq)]
pub struct SubtreeUsage {
    /// The process the subtree is rooted at, still present in this snapshot.
    pub root: ProcessIdentity,
    /// Every member, root first, then descendants in breadth-first order.
    ///
    /// Breadth-first because a truncated list should show a build's direct children
    /// before its grandchildren, and because the order is then stable under the
    /// arbitrary order the OS enumerates processes in.
    pub members: Vec<ProcessIdentity>,
    /// Summed CPU. May exceed 100% — it is a sum of core-normalized shares (§8.3).
    pub cpu: Summed<Percent>,
    /// Summed resident memory.
    ///
    /// Shared pages are counted once per process that maps them, so this over-counts a
    /// family that shares a lot — a browser especially. That is a property of RSS rather
    /// than of this sum, and [`SubtreeUsage`] cannot fix it without a
    /// proportional-set-size figure neither platform gives cheaply. Named in the docs
    /// rather than silently presented as a memory total.
    pub rss_bytes: Summed<u64>,
    /// Summed read throughput.
    pub read: Summed<Rate>,
    /// Summed write throughput.
    pub write: Summed<Rate>,
    /// Parent links that were cut to break a cycle, for the Inspect screen.
    pub cycles_broken: u32,
}

impl SubtreeUsage {
    /// Sums `root` and its descendants in `snapshot`.
    ///
    /// [`None`] when `root` is not in the snapshot — it exited, or a PID was reused and
    /// the identity no longer matches. The caller reports that as the subtree ending
    /// rather than substituting the reparented children (see the module docs).
    #[must_use]
    pub fn of(snapshot: &SystemSnapshot, root: ProcessIdentity) -> Option<Self> {
        Self::over(&snapshot.processes, root)
    }

    /// Sums over an arbitrary process list, which is what the tests and the reducer use.
    #[must_use]
    pub fn over(processes: &[ProcessSnapshot], root: ProcessIdentity) -> Option<Self> {
        let by_identity: HashMap<ProcessIdentity, &ProcessSnapshot> = processes
            .iter()
            .map(|process| (process.identity, process))
            .collect();
        by_identity.get(&root)?;

        // Children indexed by parent *PID*, because that is all a process carries — the
        // parent's start key is not in the table. A PID collision here would mean the
        // kernel reported two live processes with the same PID, which cannot happen.
        let mut children: HashMap<u32, Vec<ProcessIdentity>> = HashMap::new();
        for process in processes {
            if let Some(parent) = process.parent_pid {
                // A process whose parent is itself is the pathological `/proc` read that
                // `ProcessTree` also guards against; dropping the link here is what stops
                // the walk below from revisiting it.
                if parent != process.identity.pid {
                    children.entry(parent).or_default().push(process.identity);
                }
            }
        }
        // Deterministic order regardless of how the OS enumerated the table (§7.2).
        for siblings in children.values_mut() {
            siblings.sort_unstable_by_key(|identity| (identity.pid, identity.start_key));
        }

        let mut members = Vec::new();
        let mut seen: HashSet<ProcessIdentity> = HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut cycles_broken = 0u32;
        queue.push_back(root);
        seen.insert(root);
        while let Some(identity) = queue.pop_front() {
            members.push(identity);
            for child in children.get(&identity.pid).into_iter().flatten() {
                if seen.insert(*child) {
                    queue.push_back(*child);
                } else {
                    // Already visited: the parent links form a cycle. Counted rather than
                    // followed, so the walk terminates and the Inspect screen can say the
                    // table was inconsistent.
                    cycles_broken = cycles_broken.saturating_add(1);
                }
            }
        }

        let rows: Vec<&ProcessSnapshot> = members
            .iter()
            .filter_map(|identity| by_identity.get(identity).copied())
            .collect();

        Some(Self {
            root,
            cpu: sum_percent(&rows, |process| process.cpu),
            rss_bytes: sum_u64(&rows, |process| process.memory.rss_bytes),
            read: sum_rate(&rows, |process| process.io.read),
            write: sum_rate(&rows, |process| process.io.write),
            members,
            cycles_broken,
        })
    }

    /// How many processes the subtree holds, including the root.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the subtree holds nothing, which cannot happen: the root is a member.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Whether any visible figure understates the subtree.
    ///
    /// The flag a renderer needs next to the numbers: true means at least one sum is a
    /// lower bound, and the per-metric [`Summed::coverage`] says which and by how many
    /// members.
    #[must_use]
    pub fn has_partial_sums(&self) -> bool {
        self.cpu.is_partial()
            || self.rss_bytes.is_partial()
            || self.read.is_partial()
            || self.write.is_partial()
    }
}

/// The state an all-unavailable sum collapses to.
///
/// The *first* unavailability in member order, so a subtree where everything was refused
/// says `permission denied` rather than a generic absence. `WarmingUp` is the fallback
/// for an empty membership, which the public API cannot produce — the root is always a
/// member — but which keeps this total rather than panicking.
fn collapse<T: Copy>(states: &[MetricState<T>]) -> MetricState<T> {
    states
        .iter()
        .find(|state| state.fresh().is_none())
        .map_or(MetricState::WarmingUp, |state| match state {
            // A stale value contributed its number, so it is not a reason for the sum to
            // be unavailable; it can only be reached here if the slice is all-stale,
            // which `sum_*` never passes in.
            MetricState::Available(_) | MetricState::Stale { .. } => MetricState::WarmingUp,
            MetricState::WarmingUp => MetricState::WarmingUp,
            MetricState::PermissionDenied => MetricState::PermissionDenied,
            MetricState::Unsupported => MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(reason) => {
                MetricState::TemporarilyUnavailable(*reason)
            }
        })
}

/// Sums the percentages a member reports, counting who contributed.
///
/// A stale value *is* counted: it was measured, the renderer marks the row as stale
/// anyway, and excluding it would make a subtree's total drop every time one member's
/// read failed once. §4 allows a retained value to be used as long as its age travels
/// with it, and the coverage here is about readability rather than freshness.
fn sum_percent(
    rows: &[&ProcessSnapshot],
    pick: impl Fn(&ProcessSnapshot) -> MetricState<Percent>,
) -> Summed<Percent> {
    let states: Vec<MetricState<Percent>> = rows.iter().map(|row| pick(row)).collect();
    let mut total = 0.0f32;
    let mut contributors = 0usize;
    for state in &states {
        if let Some((percent, _)) = state.displayable() {
            total += percent.value();
            contributors += 1;
        }
    }
    Summed {
        value: if contributors == 0 {
            collapse(&states)
        } else {
            // A sum of core-normalized shares legitimately exceeds 100% (§8.3), which is
            // why `Percent` is not clamped.
            Percent::new(total).map_or(MetricState::WarmingUp, MetricState::Available)
        },
        coverage: Coverage {
            contributors,
            members: rows.len(),
        },
    }
}

/// Sums the byte counts a member reports.
fn sum_u64(
    rows: &[&ProcessSnapshot],
    pick: impl Fn(&ProcessSnapshot) -> MetricState<u64>,
) -> Summed<u64> {
    let states: Vec<MetricState<u64>> = rows.iter().map(|row| pick(row)).collect();
    let mut total = 0u64;
    let mut contributors = 0usize;
    for state in &states {
        if let Some((bytes, _)) = state.displayable() {
            total = total.saturating_add(*bytes);
            contributors += 1;
        }
    }
    Summed {
        value: if contributors == 0 {
            collapse(&states)
        } else {
            MetricState::Available(total)
        },
        coverage: Coverage {
            contributors,
            members: rows.len(),
        },
    }
}

/// Sums the rates a member reports.
fn sum_rate(
    rows: &[&ProcessSnapshot],
    pick: impl Fn(&ProcessSnapshot) -> MetricState<Rate>,
) -> Summed<Rate> {
    let states: Vec<MetricState<Rate>> = rows.iter().map(|row| pick(row)).collect();
    let mut total = 0.0f64;
    let mut contributors = 0usize;
    for state in &states {
        if let Some((rate, _)) = state.displayable() {
            total += rate.per_second();
            contributors += 1;
        }
    }
    Summed {
        value: if contributors == 0 {
            collapse(&states)
        } else {
            Rate::new(total).map_or(MetricState::WarmingUp, MetricState::Available)
        },
        coverage: Coverage {
            contributors,
            members: rows.len(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::fixtures::process;

    /// `cargo` with two compilers under it, one of which has a child of its own.
    fn build_tree() -> Vec<ProcessSnapshot> {
        vec![
            process(1, 1).name("launchd").cpu(0.1).build(),
            process(100, 100)
                .name("cargo")
                .parent(1)
                .cpu(2.0)
                .rss(64)
                .build(),
            process(101, 101)
                .name("rustc")
                .parent(100)
                .cpu(120.0)
                .rss(2048)
                .build(),
            process(102, 102)
                .name("rustc")
                .parent(100)
                .cpu(98.0)
                .rss(1024)
                .build(),
            process(103, 103)
                .name("cc")
                .parent(101)
                .cpu(30.0)
                .rss(256)
                .build(),
            // A sibling of the root, which must not be counted.
            process(200, 200)
                .name("zsh")
                .parent(1)
                .cpu(0.5)
                .rss(8)
                .build(),
        ]
    }

    #[test]
    fn a_subtree_sums_the_root_and_every_descendant_but_nothing_else() {
        let usage = SubtreeUsage::over(&build_tree(), ProcessIdentity::new(100, 100))
            .expect("the root is present");

        assert_eq!(usage.len(), 4, "cargo, two rustc, one cc");
        assert_eq!(
            usage.members,
            vec![
                ProcessIdentity::new(100, 100),
                ProcessIdentity::new(101, 101),
                ProcessIdentity::new(102, 102),
                ProcessIdentity::new(103, 103),
            ],
            "root first, then breadth-first"
        );
        // 2 + 120 + 98 + 30, and deliberately over 100%: a sum of core-normalized
        // shares is not a share of the machine (§8.3).
        assert_eq!(
            usage.cpu.value.fresh().map(|percent| percent.value()),
            Some(250.0)
        );
        assert_eq!(usage.rss_bytes.value.fresh(), Some(&3392));
        assert!(
            !usage.has_partial_sums(),
            "every member reported CPU and RSS, so neither figure understates"
        );
        // The fixture's platform reports no per-process I/O at all, so those sums are
        // `Unsupported` with no contributors — which is not a partial sum, because there
        // is no number there to understate.
        assert_eq!(usage.read.value, MetricState::Unsupported);
        assert!(!usage.read.is_partial());
        assert_eq!(usage.cycles_broken, 0);
    }

    #[test]
    fn a_root_that_has_exited_is_none_rather_than_its_reparented_children() {
        // The kernel gives the children to init, and a family whose common ancestor is
        // gone is not the family the user asked to watch.
        let usage = SubtreeUsage::over(&build_tree(), ProcessIdentity::new(100, 999));
        assert!(
            usage.is_none(),
            "a start key that does not match is not the root"
        );
        assert!(SubtreeUsage::over(&build_tree(), ProcessIdentity::new(4242, 1)).is_none());
    }

    #[test]
    fn a_leaf_subtree_is_just_itself() {
        let usage =
            SubtreeUsage::over(&build_tree(), ProcessIdentity::new(103, 103)).expect("present");
        assert_eq!(usage.len(), 1);
        assert_eq!(usage.cpu.value.fresh().map(|p| p.value()), Some(30.0));
        assert!(!usage.has_partial_sums());
    }

    #[test]
    fn a_member_that_refuses_its_metric_leaves_the_sum_incomplete_rather_than_wrong() {
        let mut processes = build_tree();
        // The second compiler belongs to somebody else.
        if let Some(row) = processes.get_mut(3) {
            row.cpu = MetricState::PermissionDenied;
        }
        let usage = SubtreeUsage::over(&processes, ProcessIdentity::new(100, 100)).expect("root");

        assert_eq!(
            usage.cpu.value.fresh().map(|p| p.value()),
            Some(152.0),
            "the readable members still sum"
        );
        assert!(usage.cpu.is_partial(), "and the figure says it understates");
        assert_eq!(usage.cpu.coverage.missing(), 1);
        assert_eq!(usage.cpu.coverage.contributors, 3);
        assert_eq!(usage.cpu.coverage.members, 4);
        // Partiality is per metric, not per subtree: a process may refuse its CPU and
        // report its memory, and the memory total is then exact.
        assert!(!usage.rss_bytes.is_partial());
        assert!(usage.has_partial_sums());
    }

    #[test]
    fn a_subtree_where_nothing_can_be_read_is_unavailable_and_never_zero() {
        // §4/§26: this is the case a naive sum turns into `0`, which reads as "this
        // build is using no CPU at all".
        let mut processes = build_tree();
        for row in &mut processes {
            row.cpu = MetricState::PermissionDenied;
        }
        let usage = SubtreeUsage::over(&processes, ProcessIdentity::new(100, 100)).expect("root");

        assert_eq!(usage.cpu.value, MetricState::PermissionDenied);
        assert!(usage.cpu.value.fresh().is_none());
        assert_eq!(usage.cpu.coverage.contributors, 0);
        assert_eq!(usage.cpu.coverage.share(), Some(Percent::ZERO));
        // And with no number on screen there is nothing to mark as understating: the
        // state itself is the whole message.
        assert!(!usage.cpu.is_partial());
    }

    #[test]
    fn a_warming_up_subtree_says_so_rather_than_reporting_a_zero_total() {
        let mut processes = build_tree();
        for row in &mut processes {
            row.cpu = MetricState::WarmingUp;
        }
        let usage = SubtreeUsage::over(&processes, ProcessIdentity::new(100, 100)).expect("root");
        assert_eq!(usage.cpu.value, MetricState::WarmingUp);
    }

    #[test]
    fn a_stale_member_still_contributes_because_it_was_measured() {
        // Excluding it would make a subtree's total drop every time one member's read
        // failed once, which looks like the build getting cheaper.
        let mut processes = build_tree();
        if let Some(row) = processes.get_mut(2) {
            row.cpu = MetricState::Stale {
                value: Percent::new(120.0).expect("valid"),
                age: core::time::Duration::from_secs(2),
            };
        }
        let usage = SubtreeUsage::over(&processes, ProcessIdentity::new(100, 100)).expect("root");
        assert_eq!(usage.cpu.value.fresh().map(|p| p.value()), Some(250.0));
        assert!(!usage.cpu.is_partial());
    }

    #[test]
    fn a_parent_cycle_terminates_and_is_counted() {
        // A `/proc` read caught mid-fork can produce a loop. The walk must end.
        let processes = vec![
            process(300, 300).name("a").parent(302).cpu(1.0).build(),
            process(301, 301).name("b").parent(300).cpu(1.0).build(),
            process(302, 302).name("c").parent(301).cpu(1.0).build(),
        ];
        let usage = SubtreeUsage::over(&processes, ProcessIdentity::new(300, 300)).expect("root");
        assert_eq!(usage.len(), 3, "each process is visited once");
        assert_eq!(usage.cycles_broken, 1, "and the loop is reported");
    }

    #[test]
    fn a_self_parenting_process_is_its_own_subtree_and_no_cycle() {
        // PID 1 reporting itself as its parent is the ordinary pathological case, and
        // dropping the link is what keeps it from being reported as an inconsistency.
        let mut processes = build_tree();
        if let Some(row) = processes.get_mut(0) {
            row.parent_pid = Some(1);
        }
        let usage = SubtreeUsage::over(&processes, ProcessIdentity::new(1, 1)).expect("root");
        assert_eq!(
            usage.cycles_broken, 0,
            "a self-parent is dropped, not reported as a loop"
        );
        assert_eq!(
            usage.len(),
            6,
            "PID 1's subtree is the whole table: cargo and zsh are its children, and \
             cargo's compilers are its grandchildren"
        );
    }

    #[test]
    fn coverage_of_an_empty_membership_is_undefined_rather_than_complete() {
        let coverage = Coverage {
            contributors: 0,
            members: 0,
        };
        assert_eq!(coverage.share(), None, "a share of nothing is not 100%");
        assert_eq!(coverage.missing(), 0);
    }
}
