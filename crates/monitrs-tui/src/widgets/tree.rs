//! Tree prefixes for the process tree of §7.2 and §2.4.
//!
//! `monitrs-core` builds the tree — cycles broken, orphans re-rooted, descendants
//! counted — and hands the renderer a flat list of [`TreeRow`]s plus, for each
//! row, the continuation flags of its ancestors. All that is left is turning that
//! into characters, which is what this module does and all it does.
//!
//! # The shape
//!
//! ```text
//! systemd
//! +- sshd
//! |  `- bash
//! `- cron
//! ```
//!
//! A root has no prefix at all. Every deeper row draws one *fill* segment per
//! ancestor level below the root, then its own connector: `` `- `` when it closes
//! its sibling group and `+- ` when it does not. The fill is `| ` where the
//! ancestor at that level still has siblings below it and two spaces where it does
//! not — which is why the connector characters come from
//! [`Glyph::TreeBranch`]/[`Glyph::TreeLast`] and the fills from
//! [`Glyph::TreeVertical`]/[`Glyph::TreeIndent`], all four of which are two cells
//! wide in *both* glyph modes. That equality is asserted in `glyphs`, and it is
//! what lets a user switch `--glyphs` without every row's indentation shifting.
//!
//! # Why the root-most flag is skipped
//!
//! [`ProcessTree::continuation_flags`] returns one flag per ancestor, root-most
//! first. A depth-1 row's connector sits flush against the left edge — `+- sshd`,
//! not `  +- sshd` — so there is no fill segment for the root's level and the
//! first flag is not drawn. A depth-2 row has exactly one fill, governed by the
//! flag of the row *above* it. Skipping the first flag is therefore not an
//! off-by-one: it is the reason `|` lands under `+-` rather than one level left of
//! it.
//!
//! [`Glyph::TreeBranch`]: crate::glyphs::Glyph::TreeBranch
//! [`ProcessTree::continuation_flags`]: monitrs_core::process::ProcessTree::continuation_flags

use monitrs_core::process::{ProcessTree, TreeRow};

use crate::glyphs::{Glyph, GlyphSet};

/// Cells one indentation level occupies. Identical in both glyph modes.
pub const LEVEL_WIDTH: usize = 2;

/// Cells between a connector and the name that follows it.
pub const CONNECTOR_GAP: usize = 1;

/// The cells a prefix at `depth` needs, before any truncation.
///
/// Zero for a root, and `LEVEL_WIDTH * (depth - 1) + LEVEL_WIDTH + CONNECTOR_GAP`
/// otherwise. Saturating, so a pathologically deep tree reports `usize::MAX`
/// rather than wrapping.
#[must_use]
pub fn tree_prefix_width(depth: u32) -> usize {
    if depth == 0 {
        return 0;
    }
    let fills = usize::try_from(depth.saturating_sub(1)).unwrap_or(usize::MAX);
    fills
        .saturating_mul(LEVEL_WIDTH)
        .saturating_add(LEVEL_WIDTH)
        .saturating_add(CONNECTOR_GAP)
}

/// The prefix for one row, never wider than `max_width` cells.
///
/// `continuation_flags` is [`ProcessTree::continuation_flags`]'s output for this
/// row: one flag per ancestor, root-most first, `true` where that ancestor still
/// has siblings below it. `is_last_child` is [`TreeRow::is_last_child`].
///
/// A prefix that does not fit is truncated from the *tail*, which keeps the
/// outermost levels — the ones that say where in the tree this row is — and drops
/// the innermost. §5.7 allows a 60-cell terminal, and a 30-deep chain simply cannot
/// show its whole indentation there; losing the near levels is less confusing than
/// losing the far ones, because the row's own name is immediately to the right.
#[must_use]
pub fn tree_prefix(
    glyphs: GlyphSet,
    depth: u32,
    is_last_child: bool,
    continuation_flags: &[bool],
    max_width: usize,
) -> String {
    if depth == 0 || max_width == 0 {
        return String::new();
    }
    let connector = if is_last_child {
        glyphs.get(Glyph::TreeLast)
    } else {
        glyphs.get(Glyph::TreeBranch)
    };
    let vertical = glyphs.get(Glyph::TreeVertical);
    let indent = glyphs.get(Glyph::TreeIndent);

    let mut out = String::with_capacity(tree_prefix_width(depth).min(max_width) + connector.len());
    let mut width = 0usize;

    // One fill per ancestor level below the root; the root's own level carries no
    // fill because a depth-1 connector is flush left.
    let fill_levels = usize::try_from(depth.saturating_sub(1)).unwrap_or(usize::MAX);
    for level in 0..fill_levels {
        if width + LEVEL_WIDTH + LEVEL_WIDTH + CONNECTOR_GAP > max_width {
            // No room for this fill *and* the connector: stop filling and let the
            // connector take the remaining cells.
            break;
        }
        // `flags[0]` describes the root's level, which has no fill, so level `i`
        // reads `flags[i + 1]`.
        let continues = continuation_flags.get(level + 1).copied().unwrap_or(false);
        out.push_str(if continues { vertical } else { indent });
        width += LEVEL_WIDTH;
    }

    if width + LEVEL_WIDTH > max_width {
        // Not even the connector fits. A lone gap character would say nothing, so
        // the prefix is dropped entirely and the name starts at the column edge.
        out.clear();
        return out;
    }
    out.push_str(connector);
    width += LEVEL_WIDTH;
    if width + CONNECTOR_GAP <= max_width {
        out.push(' ');
    }
    out
}

/// The prefix for `row`, resolved against the tree it belongs to.
#[must_use]
pub fn tree_row_prefix(
    glyphs: GlyphSet,
    tree: &ProcessTree,
    index: usize,
    max_width: usize,
) -> String {
    let Some(row) = tree.row(index) else {
        return String::new();
    };
    let flags = tree.continuation_flags(index);
    tree_prefix(glyphs, row.depth, row.is_last_child, &flags, max_width)
}

/// Prefixes for every row of `tree`, in display order.
///
/// Computed in one pass so the process table can index prefixes by row rather than
/// recomputing ancestry per frame (§16.1: no per-row tree walk on the render path).
#[must_use]
pub fn tree_prefixes(glyphs: GlyphSet, tree: &ProcessTree, max_width: usize) -> Vec<String> {
    (0..tree.len())
        .map(|index| tree_row_prefix(glyphs, tree, index, max_width))
        .collect()
}

/// The prefix a single detached [`TreeRow`] needs, given its continuation flags.
///
/// A convenience for callers that already hold both, such as a screen rendering a
/// window of rows without keeping the whole tree.
#[must_use]
pub fn prefix_for_row(
    glyphs: GlyphSet,
    row: &TreeRow,
    continuation_flags: &[bool],
    max_width: usize,
) -> String {
    tree_prefix(
        glyphs,
        row.depth,
        row.is_last_child,
        continuation_flags,
        max_width,
    )
}

#[cfg(test)]
mod tests {
    use monitrs_core::model::ProcessIdentity;
    use monitrs_core::process::{ProcessSort, ProcessSortKey};
    use monitrs_core::units::display_width;

    use super::*;

    /// The §7.2 tree the module doc draws, built through `monitrs-core` so the
    /// flags are the real ones rather than hand-written.
    fn sample_tree() -> (Vec<monitrs_core::model::ProcessSnapshot>, ProcessTree) {
        use monitrs_core::model::{
            MetricState, ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState,
        };

        let make = |pid: u32, name: &str, parent: Option<u32>| ProcessSnapshot {
            identity: ProcessIdentity::new(pid, u64::from(pid)),
            parent_pid: parent,
            name: name.into(),
            command: name.into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Sleeping,
            cpu: MetricState::WarmingUp,
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        };
        let processes = vec![
            make(1, "systemd", None),
            make(2, "sshd", Some(1)),
            make(3, "bash", Some(2)),
            make(4, "cron", Some(1)),
        ];
        let tree = ProcessTree::build(&processes, ProcessSort::ascending(ProcessSortKey::Pid));
        (processes, tree)
    }

    fn shape(glyphs: GlyphSet, max_width: usize) -> Vec<String> {
        let (processes, tree) = sample_tree();
        tree_prefixes(glyphs, &tree, max_width)
            .into_iter()
            .zip(tree.rows())
            .map(|(prefix, row)| {
                let name = processes
                    .get(row.process_index)
                    .map_or("?", |process| process.name.as_ref());
                format!("{prefix}{name}")
            })
            .collect()
    }

    #[test]
    fn the_ascii_shape_is_the_one_the_module_documents() {
        assert_eq!(
            shape(GlyphSet::ascii(), 80),
            vec![
                "systemd".to_owned(),
                "+- sshd".to_owned(),
                "| `- bash".to_owned(),
                "`- cron".to_owned(),
            ]
        );
    }

    #[test]
    fn the_connectors_are_exactly_the_characters_section_five_one_names() {
        let ascii = GlyphSet::ascii();
        assert_eq!(tree_prefix(ascii, 1, false, &[false], 80), "+- ");
        assert_eq!(tree_prefix(ascii, 1, true, &[false], 80), "`- ");
        assert_eq!(tree_prefix(ascii, 2, true, &[false, true], 80), "| `- ");
        assert_eq!(tree_prefix(ascii, 2, true, &[false, false], 80), "  `- ");
    }

    #[test]
    fn enhanced_mode_uses_box_drawing_at_the_same_widths() {
        let unicode = GlyphSet::unicode();
        let ascii = GlyphSet::ascii();
        for (depth, is_last, flags) in [
            (1u32, false, vec![false]),
            (1, true, vec![true]),
            (3, false, vec![false, true, false]),
            (5, true, vec![true, false, true, false, true]),
        ] {
            let rich = tree_prefix(unicode, depth, is_last, &flags, 80);
            let plain = tree_prefix(ascii, depth, is_last, &flags, 80);
            assert_eq!(
                display_width(&rich),
                display_width(&plain),
                "depth {depth} changed width between glyph modes"
            );
            assert!(!rich.is_ascii(), "{rich:?} should use box drawing");
            assert!(plain.is_ascii(), "{plain:?} should be strict ASCII");
        }
    }

    #[test]
    fn a_root_has_no_prefix_at_all() {
        for glyphs in [GlyphSet::ascii(), GlyphSet::unicode()] {
            assert_eq!(tree_prefix(glyphs, 0, true, &[], 80), "");
            assert_eq!(tree_prefix(glyphs, 0, false, &[true, true], 80), "");
            assert_eq!(tree_prefix_width(0), 0);
        }
    }

    #[test]
    fn the_vertical_bar_lands_under_the_connector_above_it() {
        // This is the property the "skip the root-most flag" rule exists for: in
        // the documented shape, `bash`'s `|` sits in the same column as `sshd`'s
        // `+`, because `sshd` has a sibling below it.
        let rows = shape(GlyphSet::ascii(), 80);
        let sshd = rows.get(1).expect("sshd");
        let bash = rows.get(2).expect("bash");
        assert_eq!(sshd.find('+'), Some(0));
        assert_eq!(bash.find('|'), Some(0));
    }

    #[test]
    fn the_reported_width_matches_the_rendered_width() {
        for glyphs in [GlyphSet::ascii(), GlyphSet::unicode()] {
            for depth in 0..=20u32 {
                let flags = vec![true; usize::try_from(depth).unwrap_or(0)];
                let prefix = tree_prefix(glyphs, depth, false, &flags, usize::MAX);
                assert_eq!(
                    display_width(&prefix),
                    tree_prefix_width(depth),
                    "depth {depth}"
                );
            }
        }
    }

    #[test]
    fn a_prefix_never_exceeds_its_budget() {
        for glyphs in [GlyphSet::ascii(), GlyphSet::unicode()] {
            for depth in 0..=40u32 {
                let flags = vec![true; usize::try_from(depth).unwrap_or(0)];
                for max_width in 0..=40usize {
                    let prefix = tree_prefix(glyphs, depth, depth % 2 == 0, &flags, max_width);
                    assert!(
                        display_width(&prefix) <= max_width,
                        "depth {depth} at budget {max_width}: {prefix:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_deep_row_in_a_narrow_column_keeps_its_connector() {
        // The connector is what says "this row has a parent"; the fills only say
        // how far up. A budget too small for both keeps the connector.
        let ascii = GlyphSet::ascii();
        let flags = vec![true; 10];
        let prefix = tree_prefix(ascii, 10, true, &flags, 5);
        assert!(display_width(&prefix) <= 5);
        assert!(prefix.contains("`-"), "{prefix:?}");
        let tighter = tree_prefix(ascii, 10, true, &flags, 2);
        assert_eq!(tighter, "`-");
        let tightest = tree_prefix(ascii, 10, true, &flags, 1);
        assert_eq!(tightest, "", "one cell cannot hold a two-cell connector");
    }

    #[test]
    fn a_missing_flag_is_treated_as_no_further_siblings() {
        // A caller that supplies too few flags gets spaces rather than a panic or a
        // bar that claims a sibling exists.
        let ascii = GlyphSet::ascii();
        assert_eq!(tree_prefix(ascii, 4, true, &[], 80), "      `- ");
        assert_eq!(tree_prefix(ascii, 4, true, &[true], 80), "      `- ");
    }

    #[test]
    fn prefixes_can_be_taken_from_a_tree_or_from_a_detached_row() {
        let (_, tree) = sample_tree();
        let ascii = GlyphSet::ascii();
        assert_eq!(tree_row_prefix(ascii, &tree, 2, 80), "| `- ");
        // An index past the end is not a panic; there is simply no prefix.
        assert_eq!(tree_row_prefix(ascii, &tree, 99, 80), "");
        let row = tree.row(2).copied().expect("bash");
        let flags = tree.continuation_flags(2);
        assert_eq!(prefix_for_row(ascii, &row, &flags, 80), "| `- ");
        assert_eq!(tree_prefixes(ascii, &tree, 80).len(), tree.len());
    }

    #[test]
    fn an_empty_tree_yields_no_prefixes() {
        let tree = ProcessTree::default();
        assert!(tree_prefixes(GlyphSet::ascii(), &tree, 80).is_empty());
    }

    #[test]
    fn a_pathologically_deep_row_reports_a_saturated_width_rather_than_wrapping() {
        // The arithmetic saturates rather than wrapping, so a 4-billion-deep chain
        // reports an enormous width instead of a small one.
        assert!(tree_prefix_width(u32::MAX) > tree_prefix_width(u32::MAX - 1));
        assert!(tree_prefix_width(u32::MAX) > usize::from(u16::MAX));
        // And still renders inside its budget.
        let prefix = tree_prefix(GlyphSet::ascii(), u32::MAX, true, &[], 9);
        assert!(display_width(&prefix) <= 9, "{prefix:?}");
        assert!(prefix.ends_with("`- "), "{prefix:?}");
    }
}
