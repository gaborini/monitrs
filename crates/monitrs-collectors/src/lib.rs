//! Platform collectors for monitrs.
//!
//! # What this crate is allowed to know
//!
//! Collectors produce data and *availability*. They have no way to express
//! presentation — there is no colour, no width, and no theme anywhere in this
//! crate's API — because §10.1 puts the rendering decision entirely in
//! `monitrs-tui`. In the other direction, `monitrs-tui` has no dependency on this
//! crate and therefore cannot learn how `/proc` is parsed.
//!
//! # Structure
//!
//! * [`source::SnapshotSource`] is the contract: given a [`source::SampleTick`],
//!   produce one complete [`monitrs_core::SystemSnapshot`].
//! * [`tier::TierScheduler`] decides which sampling tiers are due (§8.6). It is
//!   pure, so tier timing is tested by advancing a fake clock.
//! * [`fake::FakeCollector`] produces deterministic snapshots for tests,
//!   benchmarks, and UI snapshots.
//!
//! # Rules that shaped the API
//!
//! * **A missing metric is not an error.** [`error::CollectorError`] cannot
//!   represent one; unavailability is a [`monitrs_core::MetricState`] on the
//!   affected field. A vanished process is expected and never logged as a
//!   warning (§14.1).
//! * **The clock belongs to the runtime.** A collector is told the measured
//!   interval and cannot assume one second (§8.1).
//! * **Collector instances are long-lived.** Several metrics are deltas, so
//!   recreating a collector each tick would both waste allocations and destroy
//!   every baseline (§9.1).
//! * **Only the requested data groups are refreshed.** Never an all-fields
//!   refresh (§9.1).
//! * **No external commands in the sampling loop.** No `ps`, `top`, `iostat`,
//!   `netstat`, `lsof`, `vm_stat`, or `system_profiler` (§3.2, §9.3).
//!
//! # Unsafe code
//!
//! Unlike `monitrs-core` and `monitrs-tui`, this crate cannot forbid `unsafe`
//! outright: the macOS collector needs documented libc calls. Unsafe is confined
//! to the platform modules, `unsafe_op_in_unsafe_fn` is denied so every unsafe
//! operation is explicit, and clippy denies an unsafe block without a `SAFETY:`
//! comment stating the invariant that makes it sound (§15.3).

#![warn(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
// A panic here would corrupt the terminal (§14.3); in tests these are the
// correct way to assert a precondition (§18.2: narrow allowances).
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod common;
pub mod error;
pub mod fake;
pub mod source;
pub mod tier;

pub use common::CommonCollector;
pub use error::CollectorError;
pub use fake::{FakeCollector, Scenario};
pub use source::{SampleTick, SnapshotSource};
pub use tier::{DueTiers, TierIntervals, TierScheduler};
