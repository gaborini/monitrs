//! The cross-platform baseline collector, built on `sysinfo`.
//!
//! §9.1 asks for a well-maintained cross-platform crate for the common baseline,
//! with native collectors added only where that baseline is insufficient. This is
//! that baseline. It is complete enough to run monitrs on its own, and native
//! enrichment layers on top of it rather than replacing it.
//!
//! Three rules from §9.1 shape the design and are easy to violate by accident:
//!
//! * **The collector instance is long-lived.** Several `sysinfo` values are
//!   deltas against the previous refresh; recreating `System` each tick would
//!   both waste allocations and silently reset every baseline to zero.
//! * **Only the requested data groups are refreshed.** There is no
//!   `refresh_all()` call anywhere in this module.
//! * **Cumulative counters go through our own rate engine.** `sysinfo` offers
//!   "bytes since the last refresh", which is not a rate: dividing it by an
//!   assumed interval is exactly what §8.1 forbids. We read the `total_*`
//!   counters instead and divide by the measured elapsed time.

mod collector;

pub use collector::CommonCollector;
/// The one staleness rule for every sensor publish site, shared with the native
/// layers.
///
/// Re-exported rather than duplicated: the native collectors read the battery on the
/// same sensor group and face the same question on every tick that did not read it,
/// and more than one answer to "what happens to a value that was not measured this
/// tick" is exactly the drift §4 exists to prevent.
pub(crate) use collector::published_sensor;
