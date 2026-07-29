//! The monitrs terminal interface: the visual design system.
//!
//! Three modules every widget depends on, each deliberately free of rendering
//! logic of its own:
//!
//! * [`glyphs`] — the strict-ASCII and enhanced-Unicode character sets (§5.1). A
//!   widget never writes a literal box-drawing or block character; it asks the
//!   resolved [`GlyphSet`] for one, which is what makes `--glyphs ascii`
//!   *provably* ASCII-clean rather than merely intended to be.
//! * [`theme`] — semantic colour tokens (§5.2, §5.3). A widget never names a
//!   colour; it names a meaning. That indirection is what lets the same widget
//!   render correctly on true colour, 256 colours, 16 colours, and with colour
//!   switched off entirely.
//! * [`layout`] — breakpoint resolution, panel geometry, and the process-table
//!   column-priority engine (§5.4, §5.7, §7.2). Every rectangle it produces lies
//!   inside its parent, and no function panics on a zero-area input — which §5.7
//!   makes a hard requirement rather than a nicety.
//!
//! These three know nothing about `SystemSnapshot`: they describe *how* to draw,
//! never *what*.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod glyphs;
pub mod layout;
pub mod theme;

pub use glyphs::{GlyphMode, GlyphSet, GlyphStyle, TerminalEnv};
pub use layout::{Breakpoint, Layout, TableLayout};
pub use theme::{ColorDepth, ColorMode, Theme, ThemeId, Token};
