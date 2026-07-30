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
//! descendants, handles, sockets, the open-descriptor list, cgroup — are on the
//! [`ProcessDetail`] that §8.6 loads on demand for the selected process only.
//!
//! The descriptor list is the most expensive of them — one syscall per descriptor —
//! so it is bounded by [`OpenFileList::MAX_LISTED`] in the collectors, and the
//! `DESCRIPTORS` row is where the panel says how many it did not list. A panel that
//! showed six of a process's four hundred descriptors without saying so would be
//! making exactly the claim §4 forbids.
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

use monitrs_core::model::{
    AncestorEntry, OpenFileList, ProcessDetail, ProcessIdentity, ProcessSnapshot,
};
use monitrs_core::units::{display_width, pad_left, pad_right, truncate_middle};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::app::{OverlayKind, detail_line_count};
use crate::theme::Token;
use crate::widgets::Presentation;
use crate::widgets::states::{
    describe, describe_age, describe_byte_rate, describe_bytes, describe_display, describe_percent,
};

use super::clock::format_timestamp;
use super::frame::{Anchor, OverlayPanel};
use super::row::{Pair, metric_field, muted, pairs, text_field};

/// The widest a descriptor's path is rendered, in cells.
///
/// The panel sizes itself from its widest line, and a path can be a kilobyte long:
/// one pathological descriptor would make the dialog ask for a thousand-cell frame
/// and pad every other row out to match it. Eighty cells is wider than any of the
/// fixed rows, so the cap only ever binds on a path — and it is truncated in the
/// *middle*, because the leading directory says which tree the file is in and the
/// trailing name says which file it is (§5.4). Presentation only: the model holds
/// the whole path (§10.1).
const PATH_CELLS: usize = 80;

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
    /// Delegates to [`crate::app::detail_line_count`] rather than counting again: the
    /// reducer clamps `j` against that number, and two implementations of one count
    /// are two chances for the last line to become unreachable.
    #[must_use]
    pub fn line_count(&self) -> usize {
        detail_line_count(self.detail)
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
            self.descriptor_summary_line(detail),
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
        lines.extend(self.descriptor_lines(detail));
        lines.push(text_field(
            presentation,
            "COLLECTED",
            &format_timestamp(detail.collected_at),
            Token::Muted,
        ));
        lines
    }

    /// The always-present row that says what the descriptor listing covers (§7.2).
    ///
    /// Always present for the same reason the `ANCESTRY` row is: §4 needs somewhere to
    /// say `permission denied` when the whole table was refused, and a platform that
    /// cannot list descriptors at all needs somewhere to say `n/a`. When the listing
    /// *is* available this row is also the only place the cap is visible, which is the
    /// difference between a panel that shows six of a process's descriptors and a
    /// panel that claims a process has six.
    fn descriptor_summary_line(&self, detail: &ProcessDetail) -> Line<'static> {
        let display = describe(&detail.open_file_list, |files: &OpenFileList| {
            let listed = files.count();
            let total = files.total();
            if files.is_complete() {
                format!("{listed} of {total} listed")
            } else {
                format!(
                    "{listed} of {total} listed, {} not listed",
                    files.not_listed()
                )
            }
        });
        metric_field(self.presentation, "DESCRIPTORS", &display)
    }

    /// One row per listed descriptor: its number, its kind, and its path (§7.2).
    ///
    /// The number is right-aligned and the kind left-padded to the widest kind
    /// *present*, so the paths line up into a column without reserving room for a
    /// kind this process does not hold (§5.4). The kind is what makes the row readable
    /// when there is no path: a socket says `socket` beside its `n/a`, so an absent
    /// path reads as "there is none" rather than as a failed read.
    fn descriptor_lines(&self, detail: &ProcessDetail) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let ellipsis = presentation.glyphs().ellipsis();
        let Some(files) = detail.open_file_list.fresh() else {
            return Vec::new();
        };
        let entries = files.entries();
        let number_cells = entries
            .iter()
            .map(|entry| display_width(&entry.descriptor.to_string()))
            .max()
            .unwrap_or(0);
        let kind_cells = entries
            .iter()
            .map(|entry| display_width(entry.kind.label()))
            .max()
            .unwrap_or(0);
        entries
            .iter()
            .map(|entry| {
                let number = pad_left(&entry.descriptor.to_string(), number_cells, ellipsis);
                let kind = pad_right(entry.kind.label(), kind_cells, ellipsis);
                pairs(
                    presentation,
                    &[
                        Pair::text("FD", format!("{number}  {kind}")),
                        Pair::metric(
                            "PATH",
                            &describe(&entry.path, |path| {
                                truncate_middle(path, PATH_CELLS, ellipsis)
                            }),
                        ),
                    ],
                )
            })
            .collect()
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

    use monitrs_core::model::{
        MetricState, OpenFileEntry, OpenFileKind, OpenFileList, ProcessIo, ProcessMemory,
        ProcessState, UserIdentity,
    };
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

    /// A descriptor listing with one of every state a real walk produces: a named
    /// file, a socket with no path, a descriptor the OS refused, and a total larger
    /// than the number of entries so the cap is exercised.
    fn descriptors() -> OpenFileList {
        OpenFileList::listed(
            vec![
                OpenFileEntry {
                    descriptor: 0,
                    kind: OpenFileKind::File,
                    path: MetricState::Available("/dev/null".into()),
                },
                OpenFileEntry {
                    descriptor: 4,
                    kind: OpenFileKind::Socket,
                    path: MetricState::Unsupported,
                },
                OpenFileEntry {
                    descriptor: 11,
                    kind: OpenFileKind::File,
                    path: MetricState::PermissionDenied,
                },
            ],
            42,
        )
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
        detail.open_file_list = MetricState::Available(descriptors());
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
        assert!(
            text.contains("DESCRIPTORS"),
            "listing summary missing:\n{text}"
        );
        assert!(
            text.contains("/dev/null"),
            "descriptor path missing:\n{text}"
        );
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
    fn a_capped_descriptor_listing_says_how_many_it_did_not_list() {
        // §4: showing three of forty-two descriptors without saying so would claim the
        // process has three.
        let process = process();
        let detail = detail();
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let text = full(overlay);
        let row = text
            .lines()
            .find(|line| line.contains("DESCRIPTORS"))
            .expect("the descriptor summary row");
        assert!(row.contains("3 of 42 listed"), "{row}");
        assert!(row.contains("39 not listed"), "{row}");
    }

    #[test]
    fn a_complete_descriptor_listing_does_not_claim_anything_was_left_out() {
        let process = process();
        let mut detail = detail();
        detail.open_file_list = MetricState::Available(OpenFileList::listed(
            vec![OpenFileEntry {
                descriptor: 3,
                kind: OpenFileKind::File,
                path: MetricState::Available("/etc/hosts".into()),
            }],
            1,
        ));
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let text = full(overlay);
        let row = text
            .lines()
            .find(|line| line.contains("DESCRIPTORS"))
            .expect("the descriptor summary row");
        assert!(row.contains("1 of 1 listed"), "{row}");
        assert!(!row.contains("not listed"), "{row}");
    }

    #[test]
    fn a_descriptor_with_no_path_names_its_kind_instead_of_leaving_a_blank() {
        // The §5.2 pairing applied to a socket: `n/a` alone would read as a failed
        // read, and the kind is what says there was never a path to read.
        let process = process();
        let detail = detail();
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        let text = full(overlay);

        let socket = text
            .lines()
            .find(|line| line.contains("socket"))
            .expect("the socket's row");
        assert!(socket.contains("FD"), "{socket}");
        assert!(socket.contains("- n/a"), "{socket}");
        assert!(
            !socket.contains('0'),
            "a missing path is not a zero: {socket}"
        );

        let refused = text
            .lines()
            .find(|line| line.contains("11") && line.contains("file"))
            .expect("the refused descriptor's row");
        assert!(refused.contains("! permission denied"), "{refused}");
    }

    #[test]
    fn a_refused_descriptor_table_is_one_honest_row_rather_than_no_rows() {
        // §4 needs somewhere to say it. A platform that cannot list descriptors and a
        // process that refuses them both produce zero entries, and the summary row is
        // the only thing that tells them apart.
        let process = process();
        for (state, expected) in [
            (MetricState::PermissionDenied, "permission denied"),
            (MetricState::Unsupported, "n/a"),
        ] {
            let mut detail = detail();
            detail.open_file_list = state;
            let overlay = ProcessDetailOverlay::new(
                presentation(),
                identity(),
                Some(&process),
                Some(&detail),
            );
            let text = full(overlay);
            let row = text
                .lines()
                .find(|line| line.contains("DESCRIPTORS"))
                .expect("the descriptor summary row");
            assert!(row.contains(expected), "{row}");
            assert!(!row.contains("listed"), "{row}");
        }
    }

    #[test]
    fn a_pathological_path_does_not_widen_the_panel_without_bound() {
        // §5.4, and a practical matter: the panel sizes itself from its widest line, so
        // one kilobyte-long path would pad every other row out to a kilobyte.
        let process = process();
        let mut detail = detail();
        let long: String = std::iter::repeat_n("/very-long-directory-name", 60).collect();
        detail.open_file_list = MetricState::Available(OpenFileList::listed(
            vec![OpenFileEntry {
                descriptor: 3,
                kind: OpenFileKind::File,
                path: MetricState::Available(format!("{long}/target.bin").into()),
            }],
            1,
        ));
        let overlay =
            ProcessDetailOverlay::new(presentation(), identity(), Some(&process), Some(&detail));
        assert!(
            overlay.desired_width() < 140,
            "the panel wanted {} cells",
            overlay.desired_width()
        );
        let text = full(overlay);
        // Middle truncation keeps both ends, so the file at the end is still visible.
        assert!(text.contains("target.bin"), "{text}");
        assert!(text.contains("..."), "{text}");
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
