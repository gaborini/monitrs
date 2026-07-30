//! Process list filtering (§7.2).
//!
//! §7.2 requires a "text filter over name, command, PID, and user" plus a
//! user-only toggle and a hide-kernel-threads toggle. It also permits an
//! "optional regex filter only if clearly marked and safely compiled".
//!
//! # Why there is an enum with one variant
//!
//! Regex is *not* implemented here. §13 keeps the dependency list deliberately
//! small and a regex engine cannot be written to a defensible standard as a side
//! quest inside this module, so the only pattern kind that exists today is
//! [`FilterPattern::Plain`]. The type is still an enum, and
//! [`FilterPattern::kind_label`] already exists, so adding `Regex` later is a new
//! variant rather than a new signature at every call site — and the "clearly
//! marked" half of §7.2 is satisfied by construction because the UI has a label to
//! render from day one.
//!
//! # Matching rules
//!
//! * The query is a **literal** substring. There are no wildcards, no anchors, and
//!   no escape characters, so a user searching for `[` or `.*` finds exactly that.
//!   §12 requires deterministic parsing; the most deterministic parse of a search
//!   box is "the text you typed".
//! * Matching is **case-insensitive** via simple lowercase folding, which is
//!   allocation-free for the ASCII queries that make up practically all of them.
//! * Numeric fields (PID, and a uid with no resolvable name) match against their
//!   decimal text, so `184` finds PID `31842`.
//! * A field with **no value never matches** (§26): an unattributable owner is not
//!   a match for any user query, and it is not a match for the empty string either.

use core::fmt;
use std::borrow::Borrow;

use crate::model::{MetricState, ProcessIdentity, ProcessSnapshot, UserIdentity};

/// A `u32` renders to at most ten decimal digits.
const MAX_U32_DIGITS: usize = 10;

/// A literal, case-insensitive substring pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlainPattern {
    /// The query exactly as typed, for redisplay in the filter bar.
    source: Box<str>,
    /// The query lowercased once, so matching does not refold it per row.
    folded: Box<str>,
    /// Whether `folded` is pure ASCII, which unlocks the allocation-free path.
    folded_is_ascii: bool,
}

impl PlainPattern {
    /// Builds a pattern, or `None` when `query` is empty.
    ///
    /// Only a completely empty query means "no filter". Whitespace is *not*
    /// trimmed: a trailing space is a meaningful way to distinguish `rustc ` from
    /// `rustcx`, and silently editing the user's query would make the same
    /// keystrokes produce different results depending on hidden rules.
    #[must_use]
    pub fn new(query: &str) -> Option<Self> {
        if query.is_empty() {
            return None;
        }
        let folded = query.to_lowercase();
        Some(Self {
            source: query.into(),
            folded_is_ascii: folded.is_ascii(),
            folded: folded.into(),
        })
    }

    /// The query exactly as typed.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether `haystack` contains this pattern, ignoring case.
    #[must_use]
    pub fn matches_text(&self, haystack: &str) -> bool {
        if self.folded_is_ascii {
            // An ASCII needle can be searched byte-wise even in a haystack with
            // multi-byte characters: every byte of a non-ASCII character is >= 0x80
            // and so can never equal an ASCII byte, in either case.
            contains_ignoring_ascii_case(haystack, &self.folded)
        } else {
            // A non-ASCII needle needs real folding, which allocates. Rare enough
            // to pay for, and still correct for the common accented-name case.
            haystack.to_lowercase().contains(&*self.folded)
        }
    }

    /// Whether the decimal rendering of `value` contains this pattern.
    #[must_use]
    pub fn matches_number(&self, value: u32) -> bool {
        self.matches_text(Decimal::new(value).as_str())
    }

    /// Whether any of the four §7.2 fields of `process` match.
    ///
    /// The user field is matched on the text the `USER` column shows — the
    /// resolved name when there is one, the uid otherwise — so what matches is
    /// what the user can see.
    #[must_use]
    pub fn matches_process(&self, process: &ProcessSnapshot) -> bool {
        self.matches_text(&process.name)
            || self.matches_text(process.command_or_name())
            || self.matches_number(process.identity.pid)
            || self.matches_user(&process.user)
    }

    /// Whether the owner column of a process matches.
    fn matches_user(&self, user: &MetricState<UserIdentity>) -> bool {
        // `displayable` rather than `fresh`: a uid does not change during a
        // process's life, so a retained owner is still the right answer. An owner
        // that was never read is not a match for anything (§26).
        match user.displayable() {
            Some((identity, _)) => match &identity.name {
                Some(name) => self.matches_text(name),
                None => self.matches_number(identity.uid),
            },
            None => false,
        }
    }
}

impl fmt::Display for PlainPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// How a filter query is interpreted.
///
/// See the module documentation for why this enum has a single variant today.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FilterPattern {
    /// Literal, case-insensitive substring matching (§7.2).
    Plain(PlainPattern),
}

impl FilterPattern {
    /// Builds a plain pattern, or `None` when `query` is empty.
    #[must_use]
    pub fn plain(query: &str) -> Option<Self> {
        PlainPattern::new(query).map(Self::Plain)
    }

    /// The query exactly as typed.
    #[must_use]
    pub fn source(&self) -> &str {
        match self {
            Self::Plain(pattern) => pattern.source(),
        }
    }

    /// A short label naming the matching mode.
    ///
    /// §7.2 allows a non-literal filter "only if clearly marked"; this is the
    /// marking, and it exists now so no future variant can be added without one.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::Plain(_) => "plain",
        }
    }

    /// Whether `process` matches.
    #[must_use]
    pub fn matches_process(&self, process: &ProcessSnapshot) -> bool {
        match self {
            Self::Plain(pattern) => pattern.matches_process(process),
        }
    }

    /// Whether `haystack` matches, for callers that highlight a matched cell.
    #[must_use]
    pub fn matches_text(&self, haystack: &str) -> bool {
        match self {
            Self::Plain(pattern) => pattern.matches_text(haystack),
        }
    }
}

impl fmt::Display for FilterPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.source())
    }
}

/// One independently toggleable row test (§7.2).
///
/// Separating the three tests keeps them composable: the process screen ANDs
/// whichever are active, the Time Lens contributor list reuses only the text one,
/// and each can be unit-tested on its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessPredicate<'a> {
    /// The text query over name, command, PID, and user.
    Text(&'a FilterPattern),
    /// The user-only toggle: keep rows owned by this uid.
    OwnedBy(u32),
    /// The hide-kernel-threads toggle (Linux; §7.2).
    NotKernelThread,
    /// Keep only the members of one followed subtree.
    ///
    /// The membership is computed once per snapshot, by [`SubtreeUsage`], and passed in
    /// as a **sorted** slice: descendancy is not a property of a single row, so unlike
    /// the other three predicates this one cannot be decided from the row alone.
    ///
    /// It is a predicate rather than a step applied after filtering so that the existing
    /// machinery does the right thing for free — in tree mode
    /// [`ProcessTree::from_snapshot_filtered`] re-attaches a hidden process's children to
    /// their nearest surviving ancestor, which for a subtree means the followed root
    /// becomes the root of what is drawn.
    ///
    /// [`SubtreeUsage`]: crate::process::SubtreeUsage
    /// [`ProcessTree::from_snapshot_filtered`]: crate::process::ProcessTree::from_snapshot_filtered
    InSubtree(&'a [ProcessIdentity]),
}

impl ProcessPredicate<'_> {
    /// Whether `process` satisfies this predicate.
    #[must_use]
    pub fn matches(&self, process: &ProcessSnapshot) -> bool {
        match self {
            Self::Text(pattern) => pattern.matches_process(process),
            // A row whose owner could not be read is *not* known to be mine, so
            // "only my processes" hides it rather than guessing (§26).
            Self::OwnedBy(uid) => process
                .user
                .displayable()
                .is_some_and(|(identity, _)| identity.uid == *uid),
            Self::NotKernelThread => !process.is_kernel_thread,
            // Binary search rather than a linear scan: this runs once per row per
            // frame, and a followed subtree on a build host can hold hundreds of
            // members out of thousands of processes.
            Self::InSubtree(members) => members.binary_search(&process.identity).is_ok(),
        }
    }
}

/// The complete set of active row filters for the process view (§7.2).
///
/// Default is "show everything": no query, no owner restriction, kernel threads
/// visible. Every field is optional and independent, so the `/`, user-only, and
/// hide-kernel-threads controls never have to know about each other.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessFilter {
    pattern: Option<FilterPattern>,
    only_user: Option<u32>,
    hide_kernel_threads: bool,
    /// The followed subtree's members, sorted, when the view is scoped to one.
    ///
    /// Owned rather than borrowed because a `ProcessFilter` outlives any one snapshot,
    /// and recomputed whenever the rows are rebuilt: a subtree's membership changes
    /// every time the root forks or a child exits, so a cached set would show a
    /// yesterday's family.
    subtree: Option<Vec<ProcessIdentity>>,
}

impl ProcessFilter {
    /// An empty filter, which keeps every row.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses the text typed into the filter bar (`/`, §6.2).
    ///
    /// Deterministic and local: the query becomes one literal substring pattern,
    /// or no pattern at all when it is empty. This is also the parser behind the
    /// `[processes] filter` config key and `--filter` (§12).
    #[must_use]
    pub fn parse(query: &str) -> Self {
        Self {
            pattern: FilterPattern::plain(query),
            ..Self::default()
        }
    }

    /// Replaces the text pattern.
    #[must_use]
    pub fn with_pattern(mut self, pattern: Option<FilterPattern>) -> Self {
        self.pattern = pattern;
        self
    }

    /// Restricts rows to one owner, or lifts the restriction with `None`.
    #[must_use]
    pub fn with_only_user(mut self, uid: Option<u32>) -> Self {
        self.only_user = uid;
        self
    }

    /// Sets the hide-kernel-threads toggle.
    #[must_use]
    pub fn with_hidden_kernel_threads(mut self, hidden: bool) -> Self {
        self.hide_kernel_threads = hidden;
        self
    }

    /// The active text pattern, if any.
    #[must_use]
    pub const fn pattern(&self) -> Option<&FilterPattern> {
        self.pattern.as_ref()
    }

    /// The uid rows are restricted to, if any.
    #[must_use]
    pub const fn only_user(&self) -> Option<u32> {
        self.only_user
    }

    /// Whether kernel threads are hidden.
    #[must_use]
    pub const fn hides_kernel_threads(&self) -> bool {
        self.hide_kernel_threads
    }

    /// Scopes the filter to one subtree's members, or lifts the scope with `None`.
    ///
    /// Sorts the membership, because [`ProcessPredicate::InSubtree`] binary-searches it
    /// and an unsorted slice would silently drop rows rather than fail. Sorting here
    /// rather than requiring it of the caller is the difference between an invariant and
    /// a convention.
    ///
    /// An *empty* membership is kept, not discarded: a root that has exited has no
    /// members, and the honest result is an empty table plus the notice that says why —
    /// not the whole process list back, which would look like the scope silently lifting.
    #[must_use]
    pub fn with_subtree(mut self, members: Option<Vec<ProcessIdentity>>) -> Self {
        self.subtree = members.map(|mut members| {
            members.sort_unstable();
            members
        });
        self
    }

    /// The followed subtree's members, if the view is scoped to one.
    #[must_use]
    pub fn subtree(&self) -> Option<&[ProcessIdentity]> {
        self.subtree.as_deref()
    }

    /// Whether anything is being filtered at all.
    ///
    /// The header shows a filter indicator only when this is true, so an inactive
    /// filter cannot look like a hidden reason for a short table.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.pattern.is_some()
            || self.only_user.is_some()
            || self.hide_kernel_threads
            || self.subtree.is_some()
    }

    /// The active predicates, in a fixed order.
    pub fn predicates(&self) -> impl Iterator<Item = ProcessPredicate<'_>> {
        [
            // Cheapest and most selective first: a subtree scope usually cuts the table
            // to a handful of rows, and `all` short-circuits, so the text pattern is not
            // run over three thousand processes to then discard them.
            self.subtree.as_deref().map(ProcessPredicate::InSubtree),
            self.pattern.as_ref().map(ProcessPredicate::Text),
            self.only_user.map(ProcessPredicate::OwnedBy),
            self.hide_kernel_threads
                .then_some(ProcessPredicate::NotKernelThread),
        ]
        .into_iter()
        .flatten()
    }

    /// Whether `process` passes every active predicate.
    #[must_use]
    pub fn matches(&self, process: &ProcessSnapshot) -> bool {
        self.predicates()
            .all(|predicate| predicate.matches(process))
    }

    /// The indices of the rows that pass the whole filter.
    #[must_use]
    pub fn match_indices<P: Borrow<ProcessSnapshot>>(&self, rows: &[P]) -> Vec<usize> {
        rows.iter()
            .map(Borrow::borrow)
            .enumerate()
            .filter(|(_, process)| self.matches(process))
            .map(|(index, _)| index)
            .collect()
    }

    /// The indices of the rows matching the *text pattern only*, for `n`/`N`
    /// navigation (§6.2).
    ///
    /// Deliberately ignores the two toggles: they scope which rows are on screen,
    /// while a search moves the selection between rows that are already on screen.
    /// Returns nothing when no pattern is set, so `n` with an empty search box is a
    /// no-op instead of jumping to the first row.
    #[must_use]
    pub fn text_match_indices<P: Borrow<ProcessSnapshot>>(&self, rows: &[P]) -> Vec<usize> {
        let Some(pattern) = self.pattern.as_ref() else {
            return Vec::new();
        };
        rows.iter()
            .map(Borrow::borrow)
            .enumerate()
            .filter(|(_, process)| pattern.matches_process(process))
            .map(|(index, _)| index)
            .collect()
    }

    /// The next text match after `from`, wrapping at the end of the list (`n`).
    ///
    /// `rows` must be in display order. `from` is the currently selected row, or
    /// `None` when nothing is selected; an out-of-range `from` is treated as no
    /// selection rather than panicking, because the selection and the row list are
    /// updated by different events.
    #[must_use]
    pub fn next_match<P: Borrow<ProcessSnapshot>>(
        &self,
        rows: &[P],
        from: Option<usize>,
    ) -> Option<usize> {
        let pattern = self.pattern.as_ref()?;
        let length = rows.len();
        if length == 0 {
            return None;
        }
        let start = match from {
            Some(index) if index < length => (index + 1) % length,
            _ => 0,
        };
        (0..length)
            .map(|offset| (start + offset) % length)
            .find(|&index| matches_at(rows, index, pattern))
    }

    /// The previous text match before `from`, wrapping at the start (`N`).
    ///
    /// With no selection this returns the *last* match, which is what wrapping
    /// backwards from the top of the list means.
    #[must_use]
    pub fn previous_match<P: Borrow<ProcessSnapshot>>(
        &self,
        rows: &[P],
        from: Option<usize>,
    ) -> Option<usize> {
        let pattern = self.pattern.as_ref()?;
        let length = rows.len();
        if length == 0 {
            return None;
        }
        let start = match from {
            Some(index) if index < length => index,
            _ => 0,
        };
        (1..=length)
            .map(|offset| (start + length - (offset % length)) % length)
            .find(|&index| matches_at(rows, index, pattern))
    }
}

/// Whether the row at `index` matches, without indexing that could panic.
fn matches_at<P: Borrow<ProcessSnapshot>>(
    rows: &[P],
    index: usize,
    pattern: &FilterPattern,
) -> bool {
    rows.get(index)
        .is_some_and(|row| pattern.matches_process(row.borrow()))
}

/// Whether `haystack` contains `needle`, comparing ASCII case-insensitively.
///
/// Allocation-free, which matters because this runs over four fields of every row
/// on every keystroke and every tick (§16.1).
fn contains_ignoring_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    let haystack = haystack.as_bytes();
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// The decimal digits of a `u32`, rendered into a stack buffer.
///
/// PID and uid matching needs the digits as text; formatting into a `String` per
/// row per tick is exactly the kind of avoidable allocation §16.1 rules out.
struct Decimal {
    digits: [u8; MAX_U32_DIGITS],
    start: usize,
}

impl Decimal {
    /// Renders `value`, filling the buffer from the right.
    fn new(value: u32) -> Self {
        let mut digits = [b'0'; MAX_U32_DIGITS];
        let mut start = MAX_U32_DIGITS;
        let mut remaining = value;
        while start > 0 {
            start -= 1;
            let digit = u8::try_from(remaining % 10).unwrap_or(0);
            if let Some(slot) = digits.get_mut(start) {
                *slot = b'0'.saturating_add(digit);
            }
            remaining /= 10;
            if remaining == 0 {
                break;
            }
        }
        Self { digits, start }
    }

    /// The rendered digits.
    fn as_str(&self) -> &str {
        // Every byte written is an ASCII digit, so this cannot fail; the fallback
        // keeps the promise that nothing in a production path panics.
        self.digits
            .get(self.start..)
            .and_then(|digits| core::str::from_utf8(digits).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::process;
    use super::*;
    use crate::model::UnavailableReason;

    fn table() -> Vec<ProcessSnapshot> {
        vec![
            process(1, 1)
                .name("systemd")
                .command("/sbin/init")
                .user(0, Some("root"))
                .build(),
            process(31_842, 900_100)
                .name("rustc")
                .command("rustc --edition 2024 src/lib.rs")
                .user(501, Some("gabor"))
                .build(),
            process(1_221, 2)
                .name("postgres")
                .command("postgres -D /var/lib/postgres")
                .user(70, Some("_postgres"))
                .build(),
            process(2, 3)
                .name("kthreadd")
                .user(0, Some("root"))
                .kernel_thread()
                .build(),
        ]
    }

    fn matching_pids(filter: &ProcessFilter, rows: &[ProcessSnapshot]) -> Vec<u32> {
        filter
            .match_indices(rows)
            .into_iter()
            .filter_map(|index| rows.get(index).map(|row| row.identity.pid))
            .collect()
    }

    #[test]
    fn an_empty_query_is_no_filter_at_all() {
        let filter = ProcessFilter::parse("");
        assert!(!filter.is_active());
        assert!(filter.pattern().is_none());
        assert_eq!(filter.predicates().count(), 0);
        assert_eq!(matching_pids(&filter, &table()).len(), 4);
    }

    #[test]
    fn the_name_field_matches() {
        let rows = table();
        assert_eq!(
            matching_pids(&ProcessFilter::parse("postgres"), &rows),
            vec![1_221]
        );
    }

    #[test]
    fn the_command_field_matches_arguments_the_name_does_not_contain() {
        let rows = table();
        assert_eq!(
            matching_pids(&ProcessFilter::parse("src/lib.rs"), &rows),
            vec![31_842]
        );
        assert_eq!(
            matching_pids(&ProcessFilter::parse("/sbin/init"), &rows),
            vec![1]
        );
    }

    #[test]
    fn the_pid_field_matches_as_a_decimal_substring() {
        let rows = table();
        assert_eq!(
            matching_pids(&ProcessFilter::parse("31842"), &rows),
            vec![31_842]
        );
        assert_eq!(
            matching_pids(&ProcessFilter::parse("184"), &rows),
            vec![31_842],
            "a fragment of a PID is a match, like any other substring"
        );
    }

    #[test]
    fn the_user_field_matches() {
        let rows = table();
        assert_eq!(
            matching_pids(&ProcessFilter::parse("gabor"), &rows),
            vec![31_842]
        );
        assert_eq!(
            matching_pids(&ProcessFilter::parse("_postgres"), &rows),
            vec![1_221]
        );
    }

    #[test]
    fn an_unresolved_user_name_matches_on_its_uid() {
        let rows = [process(9, 9).name("x").user(501, None).build()];
        assert!(ProcessFilter::parse("501").matches(rows.first().expect("one row")));
    }

    #[test]
    fn a_resolved_name_is_matched_instead_of_the_uid_because_that_is_what_is_shown() {
        let rows = [process(9, 9).name("x").user(501, Some("gabor")).build()];
        let row = rows.first().expect("one row");
        assert!(ProcessFilter::parse("gabor").matches(row));
        assert!(
            !ProcessFilter::parse("501").matches(row),
            "the USER column shows `gabor`, so `501` is not a visible match"
        );
    }

    #[test]
    fn an_unattributable_owner_never_matches() {
        // PIDs deliberately free of the digits searched for below, so a failure
        // means the owner matched rather than the PID column.
        let rows = [
            process(9, 9)
                .name("x")
                .user_state(MetricState::PermissionDenied)
                .build(),
            process(77, 77)
                .name("y")
                .user_state(MetricState::TemporarilyUnavailable(
                    UnavailableReason::ProcessExited,
                ))
                .build(),
        ];
        for row in &rows {
            assert!(!ProcessFilter::parse("root").matches(row));
            assert!(!ProcessFilter::parse("501").matches(row));
            assert!(!ProcessFilter::parse("0").matches(row));
        }
    }

    #[test]
    fn matching_is_case_insensitive_in_both_directions() {
        let rows = table();
        assert_eq!(
            matching_pids(&ProcessFilter::parse("RUSTC"), &rows),
            vec![31_842]
        );
        let mixed = [process(5, 5).name("WindowServer").build()];
        assert!(ProcessFilter::parse("windowserver").matches(mixed.first().expect("one row")));
        assert!(ProcessFilter::parse("WINDOWSERVER").matches(mixed.first().expect("one row")));
    }

    #[test]
    fn matching_is_case_insensitive_for_non_ascii_queries_too() {
        let rows = [process(5, 5).name("Zürich-Backup").build()];
        let row = rows.first().expect("one row");
        assert!(ProcessFilter::parse("zürich").matches(row));
        assert!(ProcessFilter::parse("ZÜRICH").matches(row));
        assert!(!ProcessFilter::parse("zurich").matches(row));
    }

    #[test]
    fn an_ascii_query_is_not_confused_by_multi_byte_characters() {
        let rows = [process(5, 5).name("héllo-world").build()];
        let row = rows.first().expect("one row");
        assert!(ProcessFilter::parse("world").matches(row));
        assert!(!ProcessFilter::parse("hello").matches(row));
    }

    #[test]
    fn the_query_is_a_literal_with_no_wildcard_or_regex_meaning() {
        let rows = vec![
            process(5, 5).name("node").command("node .*").build(),
            process(6, 6)
                .name("bash")
                .command("bash -c 'ls [ab]'")
                .build(),
        ];
        assert_eq!(
            matching_pids(&ProcessFilter::parse(".*"), &rows),
            vec![5],
            "`.*` matches only the literal text `.*`"
        );
        assert_eq!(matching_pids(&ProcessFilter::parse("[ab]"), &rows), vec![6]);
        assert!(matching_pids(&ProcessFilter::parse("^node"), &rows).is_empty());
    }

    #[test]
    fn whitespace_in_the_query_is_preserved_because_it_is_meaningful() {
        let rows = vec![
            process(5, 5).name("rustc").command("rustc --help").build(),
            process(6, 6).name("rustcx").command("rustcx").build(),
        ];
        assert_eq!(
            matching_pids(&ProcessFilter::parse("rustc "), &rows),
            vec![5]
        );
        assert_eq!(
            ProcessFilter::parse(" ")
                .pattern()
                .map(FilterPattern::source),
            Some(" ")
        );
    }

    #[test]
    fn a_kernel_thread_with_no_command_line_still_matches_on_its_name() {
        let rows = [process(2, 3).name("kworker/2:1").kernel_thread().build()];
        assert!(ProcessFilter::parse("kworker").matches(rows.first().expect("one row")));
    }

    #[test]
    fn the_hide_kernel_threads_toggle_removes_only_kernel_threads() {
        let rows = table();
        let filter = ProcessFilter::new().with_hidden_kernel_threads(true);
        assert!(filter.is_active());
        assert_eq!(matching_pids(&filter, &rows), vec![1, 31_842, 1_221]);
        assert_eq!(
            matching_pids(&ProcessFilter::new(), &rows).len(),
            4,
            "the toggle is off by default"
        );
    }

    #[test]
    fn the_user_only_toggle_keeps_one_owner() {
        let rows = table();
        let filter = ProcessFilter::new().with_only_user(Some(0));
        assert_eq!(matching_pids(&filter, &rows), vec![1, 2]);
        assert_eq!(
            matching_pids(&filter.clone().with_only_user(None), &rows).len(),
            4
        );
    }

    #[test]
    fn the_user_only_toggle_accepts_a_stale_owner_but_not_a_missing_one() {
        let stale = MetricState::Available(UserIdentity {
            uid: 501,
            name: Some("gabor".into()),
        })
        .into_stale(core::time::Duration::from_secs(5));
        let rows = vec![
            process(1, 1).user_state(stale).build(),
            process(2, 2)
                .user_state(MetricState::PermissionDenied)
                .build(),
        ];
        let filter = ProcessFilter::new().with_only_user(Some(501));
        assert_eq!(matching_pids(&filter, &rows), vec![1]);
    }

    #[test]
    fn toggles_and_text_compose_with_and_semantics() {
        let rows = table();
        let filter = ProcessFilter::parse("root")
            .with_hidden_kernel_threads(true)
            .with_only_user(Some(0));
        assert_eq!(filter.predicates().count(), 3);
        assert_eq!(matching_pids(&filter, &rows), vec![1]);
    }

    #[test]
    fn each_predicate_can_be_used_on_its_own() {
        let pattern = FilterPattern::plain("rustc").expect("non-empty");
        let rustc = process(1, 1).name("rustc").user(501, None).build();
        let kthread = process(2, 2).name("kthreadd").kernel_thread().build();

        assert!(ProcessPredicate::Text(&pattern).matches(&rustc));
        assert!(!ProcessPredicate::Text(&pattern).matches(&kthread));
        assert!(ProcessPredicate::OwnedBy(501).matches(&rustc));
        assert!(!ProcessPredicate::OwnedBy(0).matches(&rustc));
        assert!(ProcessPredicate::NotKernelThread.matches(&rustc));
        assert!(!ProcessPredicate::NotKernelThread.matches(&kthread));
    }

    #[test]
    fn navigation_returns_match_indices_in_display_order() {
        let rows = table();
        // `lib` appears in the command line of the rustc and postgres rows only.
        let filter = ProcessFilter::parse("lib");
        assert_eq!(filter.text_match_indices(&rows), vec![1, 2]);
    }

    #[test]
    fn navigation_ignores_the_visibility_toggles() {
        let rows = table();
        // The toggles would hide the kernel thread from the table, but `n` steps
        // between the rows the caller passes in, which are already scoped.
        let filter = ProcessFilter::parse("root").with_only_user(Some(501));
        assert_eq!(filter.text_match_indices(&rows), vec![0, 3]);
    }

    #[test]
    fn next_and_previous_wrap_around_the_list() {
        let rows = table();
        let filter = ProcessFilter::parse("lib");

        assert_eq!(filter.next_match(&rows, None), Some(1));
        assert_eq!(filter.next_match(&rows, Some(1)), Some(2));
        assert_eq!(
            filter.next_match(&rows, Some(2)),
            Some(1),
            "wraps to the first match"
        );
        assert_eq!(filter.previous_match(&rows, Some(2)), Some(1));
        assert_eq!(
            filter.previous_match(&rows, Some(1)),
            Some(2),
            "wraps to the last match"
        );
        assert_eq!(
            filter.previous_match(&rows, None),
            Some(2),
            "backwards from nothing selected is the last match"
        );
    }

    #[test]
    fn navigation_from_the_only_match_returns_that_match() {
        let rows = table();
        let filter = ProcessFilter::parse("rustc");
        assert_eq!(filter.next_match(&rows, Some(1)), Some(1));
        assert_eq!(filter.previous_match(&rows, Some(1)), Some(1));
    }

    #[test]
    fn navigation_with_no_pattern_or_no_match_moves_nowhere() {
        let rows = table();
        assert_eq!(ProcessFilter::new().next_match(&rows, Some(0)), None);
        assert_eq!(ProcessFilter::new().previous_match(&rows, None), None);
        let missing = ProcessFilter::parse("no-such-process");
        assert_eq!(missing.next_match(&rows, None), None);
        assert_eq!(missing.previous_match(&rows, None), None);
    }

    #[test]
    fn navigation_survives_an_empty_list_and_a_stale_selection_index() {
        let empty: Vec<ProcessSnapshot> = Vec::new();
        let filter = ProcessFilter::parse("rustc");
        assert_eq!(filter.next_match(&empty, Some(7)), None);
        assert_eq!(filter.previous_match(&empty, Some(7)), None);

        let rows = table();
        assert_eq!(
            filter.next_match(&rows, Some(999)),
            Some(1),
            "an out-of-range selection restarts the search rather than panicking"
        );
        assert_eq!(filter.previous_match(&rows, Some(999)), Some(1));
    }

    #[test]
    fn navigation_works_on_borrowed_rows() {
        let rows = table();
        let borrowed: Vec<&ProcessSnapshot> = rows.iter().collect();
        let filter = ProcessFilter::parse("rustc");
        assert_eq!(filter.next_match(&borrowed, None), Some(1));
        assert_eq!(filter.text_match_indices(&borrowed), vec![1]);
    }

    #[test]
    fn the_pattern_kind_is_labelled_and_the_query_is_recoverable() {
        let filter = ProcessFilter::parse("Rustc");
        let pattern = filter.pattern().expect("a pattern");
        assert_eq!(pattern.kind_label(), "plain");
        assert_eq!(pattern.source(), "Rustc");
        assert_eq!(pattern.to_string(), "Rustc");
        assert!(matches!(pattern, FilterPattern::Plain(_)));
    }

    #[test]
    fn decimal_rendering_covers_the_edges_of_the_pid_space() {
        assert_eq!(Decimal::new(0).as_str(), "0");
        assert_eq!(Decimal::new(1).as_str(), "1");
        assert_eq!(Decimal::new(31_842).as_str(), "31842");
        assert_eq!(Decimal::new(u32::MAX).as_str(), "4294967295");
    }

    #[test]
    fn ascii_insensitive_search_handles_degenerate_inputs() {
        assert!(contains_ignoring_ascii_case("anything", ""));
        assert!(!contains_ignoring_ascii_case("", "x"));
        assert!(contains_ignoring_ascii_case("abc", "abc"));
        assert!(!contains_ignoring_ascii_case("ab", "abc"));
    }
}
