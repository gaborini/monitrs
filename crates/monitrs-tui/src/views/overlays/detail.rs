//! The process detail overlay (§2.4, §7.5).
//!
//! §2.4 wants the *relationships* obvious and §7.5 lists the selected-process
//! subsection field by field. Both lists are here, and every field that a platform
//! can withhold goes through [`crate::widgets::states`], so `open files` on a
//! process the user does not own reads `! permission denied` rather than `0`.
//!
//! # Two blocks, because the data comes from two places
//!
//! The cheap fields — identity, executable, command, user, CPU, memory, disk rates,
//! threads, age — are on the [`ProcessSnapshot`] the fast tier collects for every
//! process every tick. The expensive ones — working directory, ancestry, children,
//! descendants, handles, sockets, cgroup — are on the [`ProcessDetail`] that §8.6
//! loads on demand for the selected process only.
//!
//! That split is also the scrolling split. The snapshot block is *pinned*: it is what
//! identifies the process, and losing it while scrolling would leave the user reading
//! a cgroup path without knowing whose it is. The detail block scrolls, and its line
//! count is exactly [`crate::app::detail_line_count`] — the bound the reducer clamps
//! `j` against, so the last line is reachable and no further.
//!
//! Two rows sit in the pinned block that the reducer's count does not include, and
//! deliberately so. [`crate::app::detail_line_count`] counts *one row per ancestor*
//! and one per child; the §2.4 breadcrumb and the child *summary* are separate,
//! always-present rows, and §4 needs somewhere to say `permission denied` when the
//! chain could not be read at all — a case in which the reducer counts zero rows.
//! Pinning them resolves both problems at once: the relationships §2.4 wants obvious
//! stay on screen while the rest scrolls, and the scrolling block's length still
//! matches the reducer's exactly.
//!
//! # No environment variables
//!
//! §7.5 forbids showing environment-variable values and §15.2 forbids logging them.
//! [`ProcessDetail`] has no field for them, so this overlay could not render one if
//! it tried; `nothing_here_can_show_an_environment_variable` states the intent where
//! the next reader will look for it.

use monitrs_core::model::{AncestorEntry, ProcessDetail, ProcessIdentity, ProcessSnapshot};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::OverlayKind;
use crate::theme::Token;
use crate::widgets::Presentation;
use crate::widgets::states::{
    describe, describe_age, describe_byte_rate, describe_bytes, describe_display, describe_percent,
};

use super::clock::format_timestamp;
use super::frame::{Anchor, OverlayPanel};
use super::row::{Pair, metric_field, muted, pairs, text_field};

/// The ancestry breadcrumb separator (§2.4).
///
/// A plain ASCII arrow rather than a [`crate::glyphs::Glyph`]: the glyph set has no
/// arrow, and §5.1's strict mode allows any printable 7-bit character. Using the
/// same two cells in both modes also keeps the breadcrumb's width identical, so the
/// panel does not resize when the glyph mode is cycled with `g`.
const BREADCRUMB_ARROW: &str = " -> ";

/// The detail panel for one process.
#[derive(Clone, Debug)]
pub struct ProcessDetailOverlay<'a> {
    presentation: Presentation<'a>,
    identity: ProcessIdentity,
    process: Option<&'a ProcessSnapshot>,
    detail: Option<&'a ProcessDetail>,
    scroll: usize,
}

impl<'a> ProcessDetailOverlay<'a> {
    /// A detail panel for `identity`.
    ///
    /// `process` is the row it resolves to in the displayed snapshot and `detail` is
    /// the on-demand record, if either has arrived. `detail` is only rendered when it
    /// describes `identity`: §8.6 loads it asynchronously, so a reply for the process
    /// the user has just moved off must be discarded rather than shown against the
    /// wrong row.
    #[must_use]
    pub fn new(
        presentation: Presentation<'a>,
        identity: ProcessIdentity,
        process: Option<&'a ProcessSnapshot>,
        detail: Option<&'a ProcessDetail>,
    ) -> Self {
        Self {
            presentation,
            identity,
            process,
            detail: detail.filter(|detail| detail.identity == identity),
            scroll: 0,
        }
    }

    /// Sets the first visible line of the scrolling block.
    #[must_use]
    pub const fn with_scroll(mut self, scroll: usize) -> Self {
        self.scroll = scroll;
        self
    }

    /// How many logical lines the scrolling block has.
    ///
    /// Equal to [`crate::app::detail_line_count`] for the same record, which is what
    /// the reducer clamps scrolling against.
    #[must_use]
    pub fn line_count(&self) -> usize {
        /// Rows that are always present, one per [`ProcessDetail`] scalar field plus
        /// the collection timestamp.
        const FIXED_ROWS: usize = 9;

        let Some(detail) = self.detail else {
            return 1;
        };
        let ancestry = detail
            .ancestry
            .fresh()
            .map_or(0, |entries: &Vec<AncestorEntry>| entries.len());
        let children = detail
            .children
            .fresh()
            .map_or(0, |children: &Vec<ProcessIdentity>| children.len());
        FIXED_ROWS.saturating_add(ancestry).saturating_add(children)
    }

    /// The pinned block: everything the fast tier already knows, plus the two
    /// relationship summaries §2.4 wants kept in view (§2.4, §7.5).
    #[must_use]
    pub fn identity_lines(&self) -> Vec<Line<'static>> {
        let mut lines = self.snapshot_lines();
        lines.extend(self.relationship_lines());
        lines
    }

    /// The rows derived from the fast-tier snapshot.
    fn snapshot_lines(&self) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let units = presentation.units();
        let Some(process) = self.process else {
            return vec![
                text_field(
                    presentation,
                    "IDENTITY",
                    &format!(
                        "pid {} start key {}",
                        self.identity.pid, self.identity.start_key
                    ),
                    Token::Muted,
                ),
                muted(
                    presentation,
                    "this process is no longer in the displayed sample",
                ),
            ];
        };
        // The short fields are packed onto dense rows (§5.4). Fifteen one-field rows
        // would leave nothing to scroll at §5.7's 80×24, and the fixed part of a
        // dialog earns its rows by being short.
        vec![
            pairs(
                presentation,
                &[
                    Pair::text("NAME", &*process.name),
                    Pair::text("STATE", process.state.label()),
                    Pair::metric("USER", &describe(&process.user, |user| user.display_name())),
                ],
            ),
            // §26: a PID is not an identity, and this overlay is where a user decides
            // whether the row in front of them is still the process they meant.
            text_field(
                presentation,
                "IDENTITY",
                &format!(
                    "pid {} start key {}",
                    process.identity.pid, process.identity.start_key
                ),
                Token::Muted,
            ),
            text_field(
                presentation,
                "EXECUTABLE",
                process.exe.as_deref().unwrap_or("n/a"),
                Token::Text,
            ),
            text_field(
                presentation,
                "COMMAND",
                process.command_or_name(),
                Token::Text,
            ),
            pairs(
                presentation,
                &[
                    Pair::metric("AGE", &describe_age(&process.age)),
                    Pair::metric(
                        "STARTED",
                        &describe(&process.started_at, |started| format_timestamp(*started)),
                    ),
                ],
            ),
            pairs(
                presentation,
                &[
                    Pair::metric("CPU", &describe_percent(&process.cpu)),
                    Pair::metric("RSS", &describe_bytes(&process.memory.rss_bytes, units)),
                    Pair::metric("MEM%", &describe_percent(&process.memory.share_of_total)),
                    Pair::metric("THREADS", &describe_display(&process.threads)),
                ],
            ),
            pairs(
                presentation,
                &[
                    Pair::metric("READ", &describe_byte_rate(&process.io.read, units)),
                    Pair::metric("WRITE", &describe_byte_rate(&process.io.write, units)),
                    Pair::metric(
                        "VIRTUAL",
                        &describe_bytes(&process.memory.virtual_bytes, units),
                    ),
                ],
            ),
        ]
    }

    /// The §2.4 breadcrumb and the child summary, which are always present.
    ///
    /// Always present is the point: §4 needs a row to say `permission denied` in when
    /// the chain could not be read, and §2.4 wants the relationships visible rather
    /// than several screens down.
    fn relationship_lines(&self) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let Some(detail) = self.detail else {
            return Vec::new();
        };
        vec![
            self.ancestry_line(detail),
            metric_field(
                presentation,
                "CHILDREN",
                &describe(&detail.children, |children: &Vec<ProcessIdentity>| {
                    children.len().to_string()
                }),
            ),
        ]
    }

    /// The scrolling block: the on-demand record of §8.6.
    #[must_use]
    pub fn detail_lines(&self) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let Some(detail) = self.detail else {
            // §8.6 loads this on demand, so "not here yet" is the normal first frame
            // and must not read as "this process has no working directory".
            return vec![muted(
                presentation,
                "the on-demand detail for this process has not arrived yet",
            )];
        };
        let mut lines = vec![
            metric_field(
                presentation,
                "CWD",
                &describe(&detail.working_directory, |path| path.to_string()),
            ),
            metric_field(
                presentation,
                "ROOT",
                &describe(&detail.root, |path| path.to_string()),
            ),
            metric_field(
                presentation,
                "OPEN FILES",
                &describe_display(&detail.open_files),
            ),
            metric_field(presentation, "SOCKETS", &describe_display(&detail.sockets)),
            metric_field(
                presentation,
                "DESCENDANTS",
                &describe_display(&detail.descendants),
            ),
            metric_field(presentation, "NICE", &describe_display(&detail.nice)),
            metric_field(
                presentation,
                "CGROUP",
                &describe(&detail.cgroup, |path| path.to_string()),
            ),
            metric_field(
                presentation,
                "CONTAINER",
                &describe(&detail.container, |name| name.to_string()),
            ),
        ];
        if let Some(entries) = detail.ancestry.fresh() {
            for entry in entries {
                lines.push(text_field(
                    presentation,
                    "ANCESTOR",
                    &format!("{} (pid {})", entry.name, entry.identity.pid),
                    Token::Text,
                ));
            }
        }
        if let Some(children) = detail.children.fresh() {
            for child in children {
                lines.push(text_field(
                    presentation,
                    "CHILD",
                    &format!("pid {} start key {}", child.pid, child.start_key),
                    Token::Text,
                ));
            }
        }
        lines.push(text_field(
            presentation,
            "COLLECTED",
            &format_timestamp(detail.collected_at),
            Token::Muted,
        ));
        lines
    }

    /// The §2.4 ancestry breadcrumb, oldest ancestor first.
    ///
    /// The record stores the chain from the immediate parent upwards, so it is
    /// reversed here: a breadcrumb reads towards the process, not away from it.
    fn ancestry_line(&self, detail: &ProcessDetail) -> Line<'static> {
        let display = describe(&detail.ancestry, |entries: &Vec<AncestorEntry>| {
            let mut names: Vec<&str> = entries.iter().map(|entry| &*entry.name).collect();
            names.reverse();
            if names.is_empty() {
                // A process with no ancestors is PID 1 or a kernel thread, and an
                // empty breadcrumb would look like a failed read.
                return "no parent".to_owned();
            }
            names.join(BREADCRUMB_ARROW)
        });
        metric_field(self.presentation, "ANCESTRY", &display)
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        OverlayPanel::new(self.presentation, OverlayKind::ProcessDetail.title())
            .with_trailing(format!("pid {}", self.identity.pid))
            .anchored(Anchor::Center)
            .with_pinned(self.identity_lines())
            .with_lines(self.detail_lines())
            .with_scroll(self.scroll)
    }

    /// The width the panel would like, borders included.
    #[must_use]
    pub fn desired_width(&self) -> u16 {
        self.panel().desired_width()
    }

    /// The height the panel would like, borders included.
    #[must_use]
    pub fn desired_height(&self) -> u16 {
        self.panel().desired_height()
    }
}

impl Widget for ProcessDetailOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.panel().render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::time::SystemTime;

    use monitrs_core::model::{MetricState, ProcessIo, ProcessMemory, ProcessState, UserIdentity};
    use monitrs_core::units::{Percent, Rate, display_width};

    use super::*;
    use crate::app::detail_line_count;
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn identity() -> ProcessIdentity {
        ProcessIdentity::new(31_842, 900_100)
    }

    fn process() -> ProcessSnapshot {
        ProcessSnapshot {
            identity: identity(),
            parent_pid: Some(501),
            name: "rustc".into(),
            command: "rustc --crate-name monitrs --edition 2024".into(),
            exe: Some("/usr/local/bin/rustc".into()),
            user: MetricState::Available(UserIdentity {
                uid: 501,
                name: Some("gabor".into()),
            }),
            state: ProcessState::Running,
            cpu: MetricState::Available(Percent::new(287.0).expect("valid")),
            memory: ProcessMemory {
                rss_bytes: MetricState::Available(2_814_509_056),
                virtual_bytes: MetricState::PermissionDenied,
                share_of_total: MetricState::Available(Percent::new(8.1).expect("valid")),
            },
            io: ProcessIo {
                read: MetricState::Available(Rate::new(18.0 * 1024.0 * 1024.0).expect("valid")),
                write: MetricState::WarmingUp,
                read_total_bytes: MetricState::Unsupported,
                write_total_bytes: MetricState::Unsupported,
            },
            threads: MetricState::Available(9),
            age: MetricState::Available(Duration::from_secs(43)),
            started_at: MetricState::Available(
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_363_247),
            ),
            is_kernel_thread: false,
        }
    }

    fn detail() -> ProcessDetail {
        let mut detail = ProcessDetail::pending(
            identity(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_363_290),
        );
        detail.working_directory = MetricState::Available("/Users/gabor/pgit/monitrs".into());
        detail.root = MetricState::Available("/".into());
        detail.open_files = MetricState::Available(42);
        detail.sockets = MetricState::PermissionDenied;
        detail.descendants = MetricState::Available(3);
        detail.nice = MetricState::Available(0);
        detail.cgroup = MetricState::Unsupported;
        detail.container = MetricState::Unsupported;
        detail.ancestry = MetricState::Available(vec![
            AncestorEntry {
                identity: ProcessIdentity::new(501, 5),
                name: "cargo".into(),
            },
            AncestorEntry {
                identity: ProcessIdentity::new(500, 4),
                name: "bash".into(),
            },
            AncestorEntry {
                identity: ProcessIdentity::new(1, 1),
                name: "launchd".into(),
            },
        ]);
        detail.children = MetricState::Available(vec![
            ProcessIdentity::new(31_900, 900_200),
            ProcessIdentity::new(31_901, 900_201),
        ]);
        detail
    }

    fn render(overlay: ProcessDetailOverlay<'_>, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        overlay.render(area, &mut buffer);
        (0..height)
            .map(|y| {
                let row: String = (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                    .collect();
                format!("{row}\n")
            })
            .collect()
    }

    fn full(overlay: ProcessDetailOverlay<'_>) -> String {
        let width = overlay.desired_width();
        let height = overlay.desired_height();
        render(overlay, width, height)
    }

    #[test]
    fn every_field_the_specification_lists_is_rendered() {
        // §2.4 and §7.5, clause by clause.
        let process = process();
        let detail = detail();
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let text = full(overlay);

        assert!(text.contains("rustc"), "identity missing:\n{text}");
        assert!(text.contains("900100"), "start key missing:\n{text}");
        assert!(
            text.contains("/usr/local/bin/rustc"),
            "executable missing:\n{text}"
        );
        assert!(text.contains("--crate-name"), "command missing:\n{text}");
        assert!(
            text.contains("/Users/gabor/pgit/monitrs"),
            "working directory missing:\n{text}"
        );
        assert!(
            text.contains("launchd -> bash -> cargo"),
            "ancestry breadcrumb missing:\n{text}"
        );
        assert!(text.contains("31900"), "children missing:\n{text}");
        assert!(text.contains("DESCENDANTS"), "{text}");
        assert!(text.contains("00:43"), "age missing:\n{text}");
        assert!(text.contains("gabor"), "user missing:\n{text}");
        assert!(text.contains("287%"), "CPU missing:\n{text}");
        assert!(text.contains("2.6G"), "memory missing:\n{text}");
        assert!(text.contains("18M/s"), "disk rate missing:\n{text}");
        assert!(text.contains("THREADS"), "thread count missing:\n{text}");
        assert!(text.contains("OPEN FILES"), "handles missing:\n{text}");
        assert!(text.contains("SOCKETS"), "sockets missing:\n{text}");
        assert!(text.contains("CGROUP"), "cgroup missing:\n{text}");
        assert!(text.contains("CONTAINER"), "container missing:\n{text}");
    }

    #[test]
    fn an_unavailable_field_reads_honestly_rather_than_as_zero() {
        let process = process();
        let detail = detail();
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let text = full(overlay);

        // Sockets were refused, virtual memory was refused, the write rate needs a
        // second sample, and the cgroup does not exist on this platform.
        assert!(text.contains("! permission denied"), "{text}");
        assert!(text.contains(". warming up"), "{text}");
        assert!(text.contains("- n/a"), "{text}");
        let sockets = text
            .lines()
            .find(|line| line.contains("SOCKETS"))
            .expect("the sockets row");
        assert!(sockets.contains("permission denied"), "{sockets}");
        assert!(!sockets.contains('0'), "{sockets}");
    }

    #[test]
    fn the_scrolling_line_count_is_the_count_the_reducer_clamps_against() {
        let process = process();
        let detail = detail();
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        assert_eq!(overlay.line_count(), detail_line_count(Some(&detail)));
        assert_eq!(overlay.detail_lines().len(), overlay.line_count());

        let pending = ProcessDetailOverlay::new(presentation(), identity(), Some(&process), None);
        assert_eq!(pending.line_count(), detail_line_count(None));
        assert_eq!(pending.detail_lines().len(), 1);
    }

    #[test]
    fn the_identity_block_stays_put_while_the_detail_scrolls() {
        let process = process();
        let detail = detail();
        let at = |scroll: usize| {
            let overlay = ProcessDetailOverlay::new(
                presentation(),
                identity(),
                Some(&process),
                Some(&detail),
            )
            .with_scroll(scroll);
            render(overlay, 70, 20)
        };
        let top = at(0);
        let bottom = at(detail_line_count(Some(&detail)).saturating_sub(1));
        let name_row = |text: &str| {
            text.lines()
                .find(|line| line.contains("NAME"))
                .map(ToOwned::to_owned)
        };
        assert_eq!(name_row(&top), name_row(&bottom), "the identity scrolled");
        assert_ne!(top, bottom, "nothing scrolled at all");
        assert!(bottom.contains("COLLECTED"), "{bottom}");
    }

    #[test]
    fn a_detail_for_another_process_is_discarded_rather_than_shown() {
        // §8.6 loads this asynchronously; a late reply must not be attached to the
        // row the user has moved on to.
        let process = process();
        let other = ProcessDetail::pending(ProcessIdentity::new(999, 9), SystemTime::UNIX_EPOCH);
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&other));
        assert_eq!(overlay.line_count(), 1);
        assert!(full(overlay).contains("has not arrived yet"));
    }

    #[test]
    fn a_vanished_process_is_reported_rather_than_rendered_as_blanks() {
        let overlay = ProcessDetailOverlay::new(presentation(), identity(), None, None);
        let text = full(overlay);
        assert!(text.contains("no longer in the displayed sample"), "{text}");
        assert!(text.contains("31842"), "{text}");
    }

    #[test]
    fn a_process_with_no_ancestors_says_so() {
        let process = process();
        let mut detail = detail();
        detail.ancestry = MetricState::Available(Vec::new());
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        assert!(full(overlay).contains("no parent"));
    }

    #[test]
    fn an_unavailable_ancestry_is_not_an_empty_breadcrumb() {
        let process = process();
        let mut detail = detail();
        detail.ancestry = MetricState::PermissionDenied;
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let text = full(overlay);
        let row = text
            .lines()
            .find(|line| line.contains("ANCESTRY"))
            .expect("the ancestry row");
        assert!(row.contains("permission denied"), "{row}");
        assert!(!row.contains("->"), "{row}");
    }

    #[test]
    fn nothing_here_can_show_an_environment_variable() {
        // §7.5 and §15.2. `ProcessDetail` has no field for them, so this is a
        // statement of intent for the next reader as much as a test.
        let process = process();
        let detail = detail();
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let text = full(overlay).to_uppercase();
        for forbidden in ["ENVIRON", "ENV ", "PATH=", "SECRET", "TOKEN="] {
            assert!(!text.contains(forbidden), "{forbidden} appeared:\n{text}");
        }
    }

    #[test]
    fn the_breadcrumb_arrow_is_ascii_clean_in_both_glyph_modes() {
        // §5.1: strict mode emits only printable 7-bit ASCII, and the arrow must not
        // change width between modes or the panel would resize on `g`.
        assert!(BREADCRUMB_ARROW.is_ascii());
        let process = process();
        let detail = detail();
        let ascii =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let unicode = ProcessDetailOverlay::new(
            presentation().with_glyphs(GlyphSet::unicode()),
            identity(),
            Some(&process),
            Some(&detail),
        );
        assert_eq!(ascii.desired_width(), unicode.desired_width());
    }

    #[test]
    fn the_panel_degrades_at_eighty_by_twenty_four_and_never_panics() {
        let process = process();
        let detail = detail();
        for (width, height) in [(80u16, 24u16), (60, 16), (20, 5), (1, 1), (0, 0)] {
            let overlay = ProcessDetailOverlay::new(
                presentation(),
                identity(),
                Some(&process),
                Some(&detail),
            );
            let text = render(overlay, width, height);
            for row in text.lines() {
                assert!(display_width(row) <= usize::from(width), "{row:?}");
            }
        }
    }
}
