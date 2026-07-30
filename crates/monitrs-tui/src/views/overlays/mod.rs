//! The overlay layer: the nine panels that float over a screen (§6.1, §7.6).
//!
//! ```text
//! Overlay::Help              -> help::HelpOverlay              §7.6
//! Overlay::ProcessDetail     -> detail::ProcessDetailOverlay    §2.4, §7.5
//! Overlay::FilterEdit        -> filter::FilterEditOverlay       §6.2
//! Overlay::CommandPalette    -> palette::CommandPaletteOverlay  §6.3
//! Overlay::SortSelector      -> sort::SortSelectorOverlay       §6.2
//! Overlay::ProcessAction     -> signal::ProcessActionOverlay    §6.2, §15.1
//! (a selected historical sample) -> attribution::SpikeAttributionOverlay  §2.2, §5.6
//! (the notice log)           -> notice::NoticeOverlay           §14.1, §21 M6
//! ```
//!
//! Seven of the nine correspond one-to-one with an [`crate::app::Overlay`] variant
//! and render *that value*. The remaining two are not modal: spike attribution is
//! what the Time Lens shows for the selected sample, and the notice overlay is what
//! the status area escalates to when there is something the user has to read. All
//! nine are built the same way and obey the same rules.
//!
//! # The rules an overlay obeys
//!
//! * **It renders a state machine, it does not own one.** The confirmation dialog
//!   draws an [`crate::app::ProcessActionStage`]; it cannot advance it, cannot send
//!   a signal, and cannot decide what a key means. §15.1's safety property is a
//!   property of [`crate::app::reduce`] and [`crate::action::PendingProcessAction`],
//!   and duplicating any part of it here would be duplicating the part that could
//!   drift.
//! * **Unavailable is never zero.** Every metric reaches the screen through
//!   [`crate::widgets::states`] and [`row::metric_field`], so a field the OS refused
//!   reads `! permission denied` rather than `0`.
//! * **Colour is supplementary.** Every state also carries the character
//!   [`crate::widgets::states`] derived for it (§5.2), and the two travel together
//!   because [`row::metric_field`] emits both or neither.
//! * **Nothing escapes the rectangle and zero area never panics.** All drawing goes
//!   through [`frame::OverlayPanel`], which goes through
//!   [`crate::widgets::Painter`] (§5.7).
//! * **No I/O, no clock.** An overlay is a pure function of the state it borrows
//!   and the area it is given (§10.5). Ages and offsets are read from the snapshot
//!   that produced them, never computed from `now`.
//!
//! # Degradation
//!
//! Every overlay is sized from its own content and clipped to the area it is given,
//! so the 80×24 case of §5.7 is not a separate code path: the panel simply gets
//! fewer rows, the scroll indicator appears, and the footer — which carries the
//! §6.2 confirmation key — is the last region to lose its row.

pub mod attribution;
pub mod clock;
pub mod detail;
pub mod filter;
pub mod frame;
pub mod help;
pub mod notice;
pub mod palette;
pub mod row;
pub mod signal;
pub mod sort;

pub use attribution::SpikeAttributionOverlay;
pub use detail::ProcessDetailOverlay;
pub use filter::FilterEditOverlay;
pub use frame::{Anchor, OverlayFrame, OverlayPanel};
pub use help::HelpOverlay;
pub use notice::{MAX_VISIBLE_NOTICES, NoticeOverlay};
pub use palette::CommandPaletteOverlay;
pub use signal::ProcessActionOverlay;
pub use sort::SortSelectorOverlay;
