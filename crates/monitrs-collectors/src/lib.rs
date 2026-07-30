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
//! * [`inodes`] holds the one thing both platforms do identically with their
//!   platform-specific inode reads: merge them onto the snapshot's filesystems.
//! * [`renice`] is the one platform-neutral *write*: `setpriority(2)` is POSIX and
//!   identical on both targets, so only the identity revalidation it depends on is
//!   platform-specific (§6.2, §15.1).
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
pub mod inodes;
pub mod linux;
pub mod macos;
pub mod renice;
pub mod selfstat;
pub mod source;
pub mod tier;

pub use common::CommonCollector;
pub use error::CollectorError;
pub use fake::{FakeCollector, Scenario};
pub use source::{SampleTick, SnapshotSource};
pub use tier::{DueTiers, TierIntervals, TierScheduler};

/// The most capable collector this build can offer on this machine.
///
/// **Every program that samples the real system must go through here.** Naming a
/// collector type directly is how a binary silently ends up on the bare
/// `sysinfo` baseline: it compiles, it runs, and it reports plausible numbers,
/// while `PermissionDenied` degrades into a fabricated `0` and the capability
/// flags understate what the machine can do. §9.2 asks for native enrichment
/// *by default*, so the choice belongs to one function rather than to each call
/// site.
///
/// The return type is boxed rather than an enum because the runtime is generic
/// over [`SnapshotSource`] and a `Box<dyn SnapshotSource>` is one itself (see the
/// blanket implementation in [`source`]), so no call site changes shape when a
/// platform gains a native layer.
///
/// # Errors
///
/// Whatever the chosen collector's constructor reports — a machine whose
/// baseline cannot be established at all.
#[cfg(all(target_os = "linux", feature = "linux-native"))]
pub fn platform_collector() -> Result<Box<dyn SnapshotSource>, CollectorError> {
    Ok(Box::new(linux::collector::LinuxCollector::new()?))
}

/// The most capable collector this build can offer on this machine.
///
/// See the Linux definition for why this indirection exists.
///
/// # Errors
///
/// Whatever the chosen collector's constructor reports.
#[cfg(all(target_os = "macos", feature = "macos-native"))]
pub fn platform_collector() -> Result<Box<dyn SnapshotSource>, CollectorError> {
    Ok(Box::new(macos::MacosCollector::new()?))
}

/// The cross-platform baseline, on a platform or a build with no native layer.
///
/// # Errors
///
/// Whatever [`CommonCollector::new`] reports.
#[cfg(not(any(
    all(target_os = "linux", feature = "linux-native"),
    all(target_os = "macos", feature = "macos-native")
)))]
pub fn platform_collector() -> Result<Box<dyn SnapshotSource>, CollectorError> {
    Ok(Box::new(CommonCollector::new()?))
}
