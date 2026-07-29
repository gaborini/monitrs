//! Deterministic ordering of the process table (§7.2).
//!
//! §7.2 requires "stable sorting with PID/start-time tie-breaker" and forbids row
//! selection from jumping unpredictably on each refresh. Three rules make that
//! true, and all three are pinned by tests in this file:
//!
//! 1. **Every comparison ends in an identity comparison.** Two rows with equal
//!    keys are ordered by `(pid, start_key)`, which is a property of the processes
//!    themselves rather than of the order the OS happened to enumerate them in. A
//!    refresh that returns the same processes therefore returns the same order,
//!    even when a hundred idle rows all report exactly `0%`.
//! 2. **A metric with no value never compares as zero** (§26). Rows whose value
//!    was never measured are parked at the end of the list, in *both* directions,
//!    so reversing the sort cannot fill the top of the table with blanks.
//! 3. **Only the value comparison is reversed by the direction.** The tie-break,
//!    the unavailable-last rule, and the fresh-before-stale rule are
//!    direction-independent, so `S` (reverse sort, §6.2) is a predictable
//!    operation rather than a mirror of unrelated internal rules.
//!
//! A [`MetricState::Stale`] value is ranked by the value it still displays rather
//! than treated as missing. That is deliberate: a single failed read must not
//! teleport a busy row to the bottom of the table, and the renderer already marks
//! stale cells with their age (§4), so nobody is misled. Fresh beats stale on an
//! exact tie, which keeps the ordering total.

use core::cmp::Ordering;
use core::fmt;
use core::str::FromStr;
use std::borrow::Borrow;

use crate::model::{MetricState, ProcessSnapshot, ProcessState, UserIdentity};
use crate::units::{Percent, Rate};

/// A sortable column of the process table (§7.2).
///
/// One variant per sortable column, using the same names the `[processes] sort`
/// config key and the `--sort` flag accept (§12), so a config value round-trips
/// through [`ProcessSortKey::as_str`] and [`FromStr`] without a translation table
/// somewhere else in the codebase.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ProcessSortKey {
    /// CPU percentage, core-normalized. The default, per §12.
    #[default]
    Cpu,
    /// Resident set size.
    ///
    /// Also the ordering of the `MEM%` column: the share of total memory is RSS
    /// divided by a constant, so it produces the same order and does not need a
    /// second key (§7.2 lists them as one priority level).
    Memory,
    /// Read throughput.
    Read,
    /// Write throughput.
    Write,
    /// Process id.
    Pid,
    /// Process name, then command line.
    Name,
    /// Time since the process started.
    Age,
    /// Owning user.
    User,
    /// Scheduling state.
    State,
    /// Thread count.
    Threads,
    /// Virtual size.
    Virtual,
}

impl ProcessSortKey {
    /// Every key, in the order the sort selector (`s`, §6.2) should list them.
    ///
    /// Ordered by the §7.2 column priority so the most useful sorts are the ones
    /// nearest the top of the selector.
    pub const ALL: [Self; 11] = [
        Self::Cpu,
        Self::Memory,
        Self::Name,
        Self::Pid,
        Self::User,
        Self::State,
        Self::Read,
        Self::Write,
        Self::Age,
        Self::Threads,
        Self::Virtual,
    ];

    /// The canonical names, for the "expected one of ..." half of a config error.
    pub const NAMES: &'static str =
        "cpu, memory, read, write, pid, name, age, user, state, threads, virtual";

    /// The canonical configuration name (§12).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Read => "read",
            Self::Write => "write",
            Self::Pid => "pid",
            Self::Name => "name",
            Self::Age => "age",
            Self::User => "user",
            Self::State => "state",
            Self::Threads => "threads",
            Self::Virtual => "virtual",
        }
    }

    /// A human label for the sort selector and the status line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU%",
            Self::Memory => "memory (RSS)",
            Self::Read => "read rate",
            Self::Write => "write rate",
            Self::Pid => "PID",
            Self::Name => "name",
            Self::Age => "age",
            Self::User => "user",
            Self::State => "state",
            Self::Threads => "threads",
            Self::Virtual => "virtual memory",
        }
    }
}

impl fmt::Display for ProcessSortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a sort field name is not one this build knows.
///
/// Carries the offending text so the config layer can point at the exact key
/// (§12) instead of reporting "invalid configuration".
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "unknown process sort field `{name}`; expected one of {}",
    ProcessSortKey::NAMES
)]
pub struct UnknownSortKey {
    name: Box<str>,
}

impl UnknownSortKey {
    /// The text that failed to parse.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl FromStr for ProcessSortKey {
    type Err = UnknownSortKey;

    /// Parses a sort field name.
    ///
    /// ASCII case is ignored and `-` is accepted for `_`, because a value typed on
    /// the command line and a value written in TOML should not disagree. The alias
    /// list is fixed and local: §12 requires configuration parsing to be
    /// deterministic, so there is no fuzzy matching and no locale involvement.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let normalized: String = text
            .trim()
            .chars()
            .map(|character| match character {
                '-' => '_',
                other => other.to_ascii_lowercase(),
            })
            .collect();
        match normalized.as_str() {
            "cpu" | "cpu_percent" => Ok(Self::Cpu),
            "memory" | "mem" | "rss" => Ok(Self::Memory),
            "read" | "read_rate" => Ok(Self::Read),
            "write" | "write_rate" => Ok(Self::Write),
            "pid" => Ok(Self::Pid),
            "name" | "command" | "comm" => Ok(Self::Name),
            "age" | "started" => Ok(Self::Age),
            "user" | "uid" => Ok(Self::User),
            "state" => Ok(Self::State),
            "threads" | "thread_count" => Ok(Self::Threads),
            "virtual" | "virt" | "vsz" => Ok(Self::Virtual),
            _ => Err(UnknownSortKey { name: text.into() }),
        }
    }
}

/// Which end of the ordering comes first.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SortDirection {
    /// Smallest first.
    Ascending,
    /// Largest first. The default, per §12 (`descending = true`).
    #[default]
    Descending,
}

impl SortDirection {
    /// Builds a direction from the `[processes] descending` config flag (§12).
    #[must_use]
    pub const fn from_descending(descending: bool) -> Self {
        if descending {
            Self::Descending
        } else {
            Self::Ascending
        }
    }

    /// Whether the largest value comes first.
    #[must_use]
    pub const fn is_descending(self) -> bool {
        matches!(self, Self::Descending)
    }

    /// The opposite direction, for the `S` key (§6.2).
    #[must_use]
    pub const fn reversed(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }

    /// A one-word label for the header indicator.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
        }
    }

    /// Applies this direction to a value comparison.
    const fn apply(self, ordering: Ordering) -> Ordering {
        match self {
            Self::Ascending => ordering,
            Self::Descending => ordering.reverse(),
        }
    }
}

/// A complete process table ordering: which column, and which way round.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ProcessSort {
    /// The column being sorted.
    pub key: ProcessSortKey,
    /// The direction of the value comparison.
    pub direction: SortDirection,
}

impl ProcessSort {
    /// Builds an ordering.
    #[must_use]
    pub const fn new(key: ProcessSortKey, direction: SortDirection) -> Self {
        Self { key, direction }
    }

    /// Builds a descending ordering, which is what almost every metric column
    /// wants: the interesting rows are the big ones.
    #[must_use]
    pub const fn descending(key: ProcessSortKey) -> Self {
        Self::new(key, SortDirection::Descending)
    }

    /// Builds an ascending ordering.
    #[must_use]
    pub const fn ascending(key: ProcessSortKey) -> Self {
        Self::new(key, SortDirection::Ascending)
    }

    /// The same column, sorted the other way round (`S`, §6.2).
    #[must_use]
    pub const fn reversed(self) -> Self {
        Self::new(self.key, self.direction.reversed())
    }

    /// A different column, keeping the current direction.
    ///
    /// Picking a column from the selector (`s`, §6.2) must not silently flip the
    /// direction as well; §7.2 forbids the table rearranging in ways the user did
    /// not ask for.
    #[must_use]
    pub const fn with_key(self, key: ProcessSortKey) -> Self {
        Self::new(key, self.direction)
    }

    /// Compares two processes under this ordering.
    ///
    /// A total order: reflexive, antisymmetric, and transitive, with the identity
    /// tie-break guaranteeing that only genuinely identical processes compare
    /// [`Ordering::Equal`]. That is what makes it safe to hand to
    /// [`slice::sort_by`] and what keeps two refreshes of the same table in the
    /// same order (§7.2).
    #[must_use]
    pub fn compare(&self, left: &ProcessSnapshot, right: &ProcessSnapshot) -> Ordering {
        self.compare_column(left, right)
            .then_with(|| left.identity.cmp(&right.identity))
    }

    /// Sorts rows in place under this ordering.
    ///
    /// Generic over `Borrow` so it works on an owned `Vec<ProcessSnapshot>` and on
    /// a borrowed `Vec<&ProcessSnapshot>` alike.
    pub fn sort<P: Borrow<ProcessSnapshot>>(&self, rows: &mut [P]) {
        rows.sort_by(|left, right| self.compare(left.borrow(), right.borrow()));
    }

    /// The display order of `rows`, as indices into `rows`.
    ///
    /// Returning indices keeps the published snapshot immutable (§10.4) and avoids
    /// cloning a process table that can hold ten thousand rows (§16.1).
    #[must_use]
    pub fn order<P: Borrow<ProcessSnapshot>>(&self, rows: &[P]) -> Vec<usize> {
        let mut indexed: Vec<(usize, &ProcessSnapshot)> =
            rows.iter().map(Borrow::borrow).enumerate().collect();
        indexed.sort_by(|(_, left), (_, right)| self.compare(left, right));
        indexed.into_iter().map(|(index, _)| index).collect()
    }

    /// The column comparison, before the identity tie-break.
    fn compare_column(&self, left: &ProcessSnapshot, right: &ProcessSnapshot) -> Ordering {
        let direction = self.direction;
        match self.key {
            ProcessSortKey::Cpu => {
                compare_metric(&left.cpu, &right.cpu, direction, compare_percent)
            }
            ProcessSortKey::Memory => compare_metric(
                &left.memory.rss_bytes,
                &right.memory.rss_bytes,
                direction,
                Ord::cmp,
            ),
            ProcessSortKey::Virtual => compare_metric(
                &left.memory.virtual_bytes,
                &right.memory.virtual_bytes,
                direction,
                Ord::cmp,
            ),
            ProcessSortKey::Read => {
                compare_metric(&left.io.read, &right.io.read, direction, compare_rate)
            }
            ProcessSortKey::Write => {
                compare_metric(&left.io.write, &right.io.write, direction, compare_rate)
            }
            ProcessSortKey::Threads => {
                compare_metric(&left.threads, &right.threads, direction, Ord::cmp)
            }
            ProcessSortKey::Age => compare_metric(&left.age, &right.age, direction, Ord::cmp),
            ProcessSortKey::User => {
                compare_metric(&left.user, &right.user, direction, compare_user)
            }
            ProcessSortKey::Pid => direction.apply(left.identity.pid.cmp(&right.identity.pid)),
            ProcessSortKey::Name => direction.apply(
                compare_ignoring_case(&left.name, &right.name).then_with(|| {
                    compare_ignoring_case(left.command_or_name(), right.command_or_name())
                }),
            ),
            ProcessSortKey::State => direction.apply(compare_state(left.state, right.state)),
        }
    }
}

/// Compares two metrics so that a value which was never measured never behaves
/// like a zero (§26).
///
/// The class ordering (has a value, then has none) and the age comparison are
/// deliberately outside `direction`: see the module documentation.
fn compare_metric<T, F>(
    left: &MetricState<T>,
    right: &MetricState<T>,
    direction: SortDirection,
    compare_value: F,
) -> Ordering
where
    F: Fn(&T, &T) -> Ordering,
{
    match (left.displayable(), right.displayable()) {
        (Some((left_value, left_age)), Some((right_value, right_age))) => direction
            .apply(compare_value(left_value, right_value))
            // Age is zero for a fresh value, so this puts fresh before stale on an
            // exact tie and orders two stale rows by how stale they are.
            .then_with(|| left_age.cmp(&right_age)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        // Which *kind* of unavailable a value is says nothing about magnitude, so
        // the identity tie-break orders these rows instead.
        (None, None) => Ordering::Equal,
    }
}

/// Orders two percentages.
///
/// `total_cmp` rather than `partial_cmp`: [`Percent`] validates finiteness at
/// construction so no `NaN` can reach here, and a total order is required for the
/// comparator to be sound.
fn compare_percent(left: &Percent, right: &Percent) -> Ordering {
    left.value().total_cmp(&right.value())
}

/// Orders two rates, on the same reasoning as [`compare_percent`].
fn compare_rate(left: &Rate, right: &Rate) -> Ordering {
    left.per_second().total_cmp(&right.per_second())
}

/// Orders two owners the way the `USER` column renders them.
///
/// A resolved name sorts before an unresolved one: the numeric fallback is not a
/// name, and grouping the unresolvable rows together keeps them out of the middle
/// of an alphabetical list. This is inside the value comparison rather than the
/// unavailable-last rule because the uid *is* known — only its label is missing.
fn compare_user(left: &UserIdentity, right: &UserIdentity) -> Ordering {
    match (&left.name, &right.name) {
        (Some(left_name), Some(right_name)) => {
            compare_ignoring_case(left_name, right_name).then_with(|| left.uid.cmp(&right.uid))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.uid.cmp(&right.uid),
    }
}

/// Orders two scheduling states by the `ps` letter shown in the `STATE` column.
///
/// Sorting a column by what it displays is the least surprising rule available,
/// and it has a useful side effect: because case is folded with uppercase first,
/// a descending state sort puts `Z` (zombie) at the top, which is the reason
/// anyone sorts by state (§7.2 requires zombies to stand out).
fn compare_state(left: ProcessState, right: ProcessState) -> Ordering {
    let (left_code, right_code) = (left.code(), right.code());
    left_code
        .to_ascii_lowercase()
        .cmp(&right_code.to_ascii_lowercase())
        .then_with(|| left_code.cmp(&right_code))
}

/// Orders two strings case-insensitively, falling back to an exact comparison.
///
/// Allocation-free: a process table can hold ten thousand rows and a sort makes
/// `O(n log n)` comparisons, so lowercasing into a `String` per comparison is not
/// affordable (§16.1). Folding is Rust's `char::to_lowercase`, which is simple
/// (not full) Unicode case folding — deterministic and dependency-free (§13).
fn compare_ignoring_case(left: &str, right: &str) -> Ordering {
    let mut left_chars = left.chars().flat_map(char::to_lowercase);
    let mut right_chars = right.chars().flat_map(char::to_lowercase);
    loop {
        match (left_chars.next(), right_chars.next()) {
            (Some(left_char), Some(right_char)) => match left_char.cmp(&right_char) {
                Ordering::Equal => {}
                difference => return difference,
            },
            (None, None) => return left.cmp(right),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use proptest::prelude::*;

    use super::super::fixtures::process;
    use super::*;
    use crate::model::{ProcessIdentity, UnavailableReason};

    fn identities(rows: &[ProcessSnapshot], order: &[usize]) -> Vec<ProcessIdentity> {
        order
            .iter()
            .filter_map(|&index| rows.get(index).map(|row| row.identity))
            .collect()
    }

    fn pids(rows: &[ProcessSnapshot], order: &[usize]) -> Vec<u32> {
        identities(rows, order)
            .into_iter()
            .map(|identity| identity.pid)
            .collect()
    }

    /// Every key, paired with a fixture that measures *only* that key.
    fn measured_for(key: ProcessSortKey, pid: u32, magnitude: u16) -> ProcessSnapshot {
        let fixture = process(pid, u64::from(pid));
        let wide = u64::from(magnitude);
        let float = f32::from(magnitude);
        match key {
            ProcessSortKey::Cpu => fixture.cpu(float),
            ProcessSortKey::Memory => fixture.rss(wide),
            ProcessSortKey::Virtual => fixture.virtual_bytes(wide),
            ProcessSortKey::Read => fixture.read(f64::from(magnitude)),
            ProcessSortKey::Write => fixture.write(f64::from(magnitude)),
            ProcessSortKey::Threads => fixture.threads(u32::from(magnitude)),
            ProcessSortKey::Age => fixture.age(wide),
            ProcessSortKey::User => {
                fixture.user(u32::from(magnitude), Some(&format!("u{magnitude:03}")))
            }
            // None of these can be unavailable, so the fixture is unchanged and
            // the unavailable-last test below skips them.
            ProcessSortKey::Pid | ProcessSortKey::Name | ProcessSortKey::State => fixture,
        }
        .build()
    }

    #[test]
    fn equal_keys_are_broken_by_identity_not_by_input_order() {
        let hot_low_pid = process(700, 5).cpu(50.0).build();
        let hot_high_pid = process(900, 5).cpu(50.0).build();
        let sort = ProcessSort::default();

        let mut forwards = vec![hot_low_pid.clone(), hot_high_pid.clone()];
        let mut backwards = vec![hot_high_pid, hot_low_pid];
        sort.sort(&mut forwards);
        sort.sort(&mut backwards);

        assert_eq!(
            forwards.iter().map(|p| p.identity.pid).collect::<Vec<_>>(),
            vec![700, 900]
        );
        assert_eq!(
            backwards.iter().map(|p| p.identity.pid).collect::<Vec<_>>(),
            vec![700, 900],
            "input order must not influence the result"
        );
    }

    #[test]
    fn a_reused_pid_is_ordered_by_its_start_key() {
        let old = process(4242, 100).cpu(10.0).build();
        let new = process(4242, 900).cpu(10.0).build();
        let mut rows = vec![new, old];
        ProcessSort::default().sort(&mut rows);
        assert_eq!(
            rows.iter()
                .map(|p| p.identity.start_key)
                .collect::<Vec<_>>(),
            vec![100, 900]
        );
    }

    #[test]
    fn two_refreshes_of_the_same_table_produce_the_same_order() {
        // The §7.2 anti-jumping requirement: identical values, different
        // enumeration order, identical result.
        let first: Vec<ProcessSnapshot> = (1..=6)
            .map(|pid| process(pid, u64::from(pid)).cpu(0.0).build())
            .collect();
        let second: Vec<ProcessSnapshot> = first.iter().rev().cloned().collect();
        let sort = ProcessSort::default();

        assert_eq!(
            identities(&first, &sort.order(&first)),
            identities(&second, &sort.order(&second))
        );
    }

    #[test]
    fn reversing_the_direction_does_not_reverse_the_tie_break() {
        let rows = vec![
            process(1, 1).cpu(5.0).build(),
            process(2, 2).cpu(5.0).build(),
            process(3, 3).cpu(5.0).build(),
        ];
        let descending = ProcessSort::default();
        assert_eq!(pids(&rows, &descending.order(&rows)), vec![1, 2, 3]);
        assert_eq!(
            pids(&rows, &descending.reversed().order(&rows)),
            vec![1, 2, 3],
            "tie-break is direction-independent so selection stays put"
        );
    }

    #[test]
    fn unavailable_values_sort_last_in_both_directions_for_every_key() {
        for key in ProcessSortKey::ALL {
            if matches!(
                key,
                ProcessSortKey::Pid | ProcessSortKey::Name | ProcessSortKey::State
            ) {
                // These columns are always measured; there is no unavailable case.
                continue;
            }
            let rows = vec![
                measured_for(key, 1, 1),
                process(2, 2).build(),
                measured_for(key, 3, 9),
            ];
            for direction in [SortDirection::Descending, SortDirection::Ascending] {
                let order = pids(&rows, &ProcessSort::new(key, direction).order(&rows));
                assert_eq!(
                    order.last(),
                    Some(&2),
                    "{key:?} {direction:?}: the unmeasured row must sort last"
                );
            }
        }
    }

    #[test]
    fn an_unmeasured_value_is_not_treated_as_zero() {
        let rows = vec![
            process(1, 1).cpu(0.0).build(),
            process(2, 2)
                .cpu_state(MetricState::PermissionDenied)
                .build(),
        ];
        // Ascending: 0% is the smallest measured value, yet the denied row still
        // comes after it. If "unavailable" were zero, the tie-break would decide
        // and PID 2 would be first.
        let order = ProcessSort::ascending(ProcessSortKey::Cpu).order(&rows);
        assert_eq!(pids(&rows, &order), vec![1, 2]);
    }

    #[test]
    fn every_flavour_of_unavailable_ranks_equally_and_is_ordered_by_identity() {
        let rows = vec![
            process(30, 30)
                .cpu_state(MetricState::TemporarilyUnavailable(
                    UnavailableReason::ProcessExited,
                ))
                .build(),
            process(10, 10).cpu_state(MetricState::Unsupported).build(),
            process(20, 20)
                .cpu_state(MetricState::PermissionDenied)
                .build(),
            process(40, 40).cpu_state(MetricState::WarmingUp).build(),
        ];
        let order = ProcessSort::default().order(&rows);
        assert_eq!(pids(&rows, &order), vec![10, 20, 30, 40]);
    }

    #[test]
    fn a_stale_value_keeps_its_place_instead_of_dropping_to_the_bottom() {
        let stale = MetricState::Available(Percent::new(90.0).expect("valid"))
            .into_stale(Duration::from_secs(2));
        let rows = vec![
            process(1, 1).cpu(5.0).build(),
            process(2, 2).cpu_state(stale).build(),
            process(3, 3).cpu_state(MetricState::WarmingUp).build(),
        ];
        let order = ProcessSort::default().order(&rows);
        assert_eq!(
            pids(&rows, &order),
            vec![2, 1, 3],
            "stale 90% outranks fresh 5%, and only the valueless row sorts last"
        );
    }

    #[test]
    fn fresh_beats_stale_on_an_exact_tie() {
        let stale = MetricState::Available(Percent::new(7.0).expect("valid"))
            .into_stale(Duration::from_secs(9));
        let rows = vec![
            process(9, 9).cpu_state(stale).build(),
            process(1, 1).cpu(7.0).build(),
        ];
        // Descending order, and the fresh row wins despite its higher PID losing
        // the tie-break: freshness is compared before identity.
        let order = ProcessSort::default().order(&rows);
        assert_eq!(pids(&rows, &order), vec![1, 9]);
    }

    #[test]
    fn cpu_sorts_by_magnitude_and_may_exceed_one_hundred_percent() {
        let rows = vec![
            process(1, 1).cpu(54.0).build(),
            process(2, 2).cpu(287.0).build(),
            process(3, 3).cpu(0.5).build(),
        ];
        let order = ProcessSort::default().order(&rows);
        assert_eq!(pids(&rows, &order), vec![2, 1, 3]);
    }

    #[test]
    fn memory_and_virtual_are_independent_columns() {
        let rows = vec![
            process(1, 1).rss(1_000).virtual_bytes(9_000_000).build(),
            process(2, 2).rss(9_000).virtual_bytes(1_000).build(),
        ];
        assert_eq!(
            pids(
                &rows,
                &ProcessSort::descending(ProcessSortKey::Memory).order(&rows)
            ),
            vec![2, 1]
        );
        assert_eq!(
            pids(
                &rows,
                &ProcessSort::descending(ProcessSortKey::Virtual).order(&rows)
            ),
            vec![1, 2]
        );
    }

    #[test]
    fn read_and_write_rates_are_independent_columns() {
        let rows = vec![
            process(1, 1).read(18_000_000.0).write(1.0).build(),
            process(2, 2).read(1.0).write(42_000_000.0).build(),
        ];
        assert_eq!(
            pids(
                &rows,
                &ProcessSort::descending(ProcessSortKey::Read).order(&rows)
            ),
            vec![1, 2]
        );
        assert_eq!(
            pids(
                &rows,
                &ProcessSort::descending(ProcessSortKey::Write).order(&rows)
            ),
            vec![2, 1]
        );
    }

    #[test]
    fn name_sorting_ignores_case_and_falls_back_to_the_command_line() {
        let rows = vec![
            process(1, 1).name("Zsh").build(),
            process(2, 2).name("cargo").command("cargo test").build(),
            process(3, 3).name("cargo").command("cargo build").build(),
            process(4, 4).name("apache").build(),
        ];
        let order = ProcessSort::ascending(ProcessSortKey::Name).order(&rows);
        assert_eq!(
            pids(&rows, &order),
            vec![4, 3, 2, 1],
            "apache, cargo build, cargo test, Zsh"
        );
    }

    #[test]
    fn user_sorting_puts_unresolved_names_after_resolved_ones() {
        let rows = vec![
            process(1, 1).user(0, None).build(),
            process(2, 2).user(501, Some("gabor")).build(),
            process(3, 3).user(70, Some("_postgres")).build(),
            process(4, 4)
                .user_state(MetricState::PermissionDenied)
                .build(),
        ];
        let order = ProcessSort::ascending(ProcessSortKey::User).order(&rows);
        assert_eq!(
            pids(&rows, &order),
            vec![3, 2, 1, 4],
            "_postgres, gabor, uid 0 (unnamed), then the unattributable row"
        );
    }

    #[test]
    fn state_sorting_puts_zombies_first_when_descending() {
        let rows = vec![
            process(1, 1).state(ProcessState::Sleeping).build(),
            process(2, 2).state(ProcessState::Zombie).build(),
            process(3, 3).state(ProcessState::Running).build(),
            process(4, 4)
                .state(ProcessState::UninterruptibleSleep)
                .build(),
        ];
        let order = ProcessSort::descending(ProcessSortKey::State).order(&rows);
        assert_eq!(pids(&rows, &order).first(), Some(&2));
        assert_eq!(
            pids(&rows, &order).last(),
            Some(&4),
            "D-state is the other extreme, one keypress away"
        );
    }

    #[test]
    fn pid_and_age_and_thread_columns_order_by_magnitude() {
        let rows = vec![
            process(900, 1).age(10).threads(2).build(),
            process(100, 2).age(90).threads(64).build(),
        ];
        assert_eq!(
            pids(
                &rows,
                &ProcessSort::ascending(ProcessSortKey::Pid).order(&rows)
            ),
            vec![100, 900]
        );
        assert_eq!(
            pids(
                &rows,
                &ProcessSort::descending(ProcessSortKey::Age).order(&rows)
            ),
            vec![100, 900]
        );
        assert_eq!(
            pids(
                &rows,
                &ProcessSort::descending(ProcessSortKey::Threads).order(&rows)
            ),
            vec![100, 900]
        );
    }

    #[test]
    fn sorting_an_empty_or_single_row_table_is_a_no_op() {
        let mut empty: Vec<ProcessSnapshot> = Vec::new();
        ProcessSort::default().sort(&mut empty);
        assert!(empty.is_empty());
        assert!(ProcessSort::default().order(&empty).is_empty());

        let mut single = vec![process(1, 1).build()];
        ProcessSort::default().sort(&mut single);
        assert_eq!(single.len(), 1);
    }

    #[test]
    fn sorting_works_on_borrowed_rows_too() {
        let rows = [
            process(1, 1).cpu(1.0).build(),
            process(2, 2).cpu(2.0).build(),
        ];
        let mut borrowed: Vec<&ProcessSnapshot> = rows.iter().collect();
        ProcessSort::default().sort(&mut borrowed);
        assert_eq!(
            borrowed.iter().map(|p| p.identity.pid).collect::<Vec<_>>(),
            vec![2, 1]
        );
    }

    #[test]
    fn config_field_names_round_trip() {
        for key in ProcessSortKey::ALL {
            assert_eq!(
                key.as_str().parse::<ProcessSortKey>(),
                Ok(key),
                "{key:?} does not round-trip"
            );
            assert!(ProcessSortKey::NAMES.contains(key.as_str()));
        }
    }

    #[test]
    fn field_name_parsing_accepts_documented_aliases_and_ignores_case() {
        assert_eq!("MEM".parse(), Ok(ProcessSortKey::Memory));
        assert_eq!("rss".parse(), Ok(ProcessSortKey::Memory));
        assert_eq!(" Command ".parse(), Ok(ProcessSortKey::Name));
        assert_eq!("thread-count".parse(), Ok(ProcessSortKey::Threads));
        assert_eq!("VSZ".parse(), Ok(ProcessSortKey::Virtual));
    }

    #[test]
    fn an_unknown_field_name_is_reported_with_the_offending_text() {
        let error = "cpu%".parse::<ProcessSortKey>().expect_err("not a field");
        assert_eq!(error.name(), "cpu%");
        let message = error.to_string();
        assert!(message.contains("cpu%"), "{message}");
        assert!(message.contains("virtual"), "{message}");
    }

    #[test]
    fn the_default_ordering_matches_the_documented_config_default() {
        // §12: `sort = "cpu"`, `descending = true`.
        let default = ProcessSort::default();
        assert_eq!(default.key, ProcessSortKey::Cpu);
        assert!(default.direction.is_descending());
        assert_eq!(
            SortDirection::from_descending(false),
            SortDirection::Ascending
        );
    }

    #[test]
    fn choosing_a_column_keeps_the_direction_and_reversing_keeps_the_column() {
        let sort = ProcessSort::ascending(ProcessSortKey::Name);
        assert_eq!(
            sort.with_key(ProcessSortKey::Age),
            ProcessSort::ascending(ProcessSortKey::Age)
        );
        assert_eq!(
            sort.reversed(),
            ProcessSort::descending(ProcessSortKey::Name)
        );
        assert_eq!(sort.reversed().reversed(), sort);
    }

    #[test]
    fn the_selector_lists_every_key_exactly_once() {
        let mut names: Vec<&str> = ProcessSortKey::ALL.iter().map(|key| key.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ProcessSortKey::ALL.len());
        for key in ProcessSortKey::ALL {
            assert!(!key.label().is_empty());
        }
    }

    #[test]
    fn comparison_is_antisymmetric_and_only_identical_rows_are_equal() {
        let left = process(1, 1).cpu(5.0).build();
        let right = process(2, 2).cpu(5.0).build();
        let sort = ProcessSort::default();
        assert_eq!(sort.compare(&left, &right), Ordering::Less);
        assert_eq!(sort.compare(&right, &left), Ordering::Greater);
        assert_eq!(sort.compare(&left, &left), Ordering::Equal);
    }

    proptest! {
        /// The §7.2 anti-jumping property, stated as a property test: the order of
        /// a table cannot depend on the order the collector enumerated it in.
        #[test]
        fn ordering_is_independent_of_input_order(
            rows in prop::collection::vec((1u32..40, 0u64..3, prop::option::of(0u32..4)), 1..24),
            rotation in 0usize..24,
        ) {
            let table: Vec<ProcessSnapshot> = rows
                .iter()
                .enumerate()
                .map(|(index, &(pid, start_key, cpu))| {
                    // The index keeps identities unique while values collide hard,
                    // which is exactly the situation that makes rows jump.
                    let unique = u64::try_from(index).unwrap_or(0);
                    let fixture = process(pid, start_key.wrapping_mul(1000) + unique);
                    match cpu {
                        Some(value) => fixture.cpu(f32::from(u16::try_from(value).unwrap_or(0))),
                        None => fixture,
                    }
                    .build()
                })
                .collect();

            let mut rotated = table.clone();
            let length = rotated.len();
            if length > 0 {
                rotated.rotate_left(rotation % length);
            }

            for key in ProcessSortKey::ALL {
                for direction in [SortDirection::Ascending, SortDirection::Descending] {
                    let sort = ProcessSort::new(key, direction);
                    prop_assert_eq!(
                        identities(&table, &sort.order(&table)),
                        identities(&rotated, &sort.order(&rotated)),
                        "{:?} {:?} depends on input order",
                        key,
                        direction
                    );
                }
            }
        }
    }
}
