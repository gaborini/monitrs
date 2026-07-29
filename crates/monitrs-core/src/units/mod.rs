//! Validated scalar types and the formatters that render them.
//!
//! Two rules from the specification are enforced structurally here rather than
//! by convention:
//!
//! * Raw byte counters and timestamps are integral. Only *calculated* values —
//!   [`Percent`] and [`Rate`] — use floating point, and both reject NaN,
//!   infinities, and negatives at construction (§10.4).
//! * Every formatter is bounded by a display-width budget it can never exceed,
//!   so a value crossing a unit boundary cannot reflow a column (§5.4).

mod bytes;
mod duration;
mod percent;
mod rate;
mod text;

pub use bytes::{
    ByteSizeParseError, ByteUnits, MAX_BYTE_RATE_WIDTH, MAX_COMPACT_BYTES_WIDTH, format_byte_rate,
    format_bytes, format_bytes_compact, parse_bytes,
};
pub use duration::{
    DurationParseError, format_age, format_duration, format_history_offset, format_uptime,
    parse_duration,
};
pub use percent::Percent;
pub use rate::Rate;
pub use text::{Ellipsis, display_width, pad_left, pad_right, truncate_middle, truncate_tail};
