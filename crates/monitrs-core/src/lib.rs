//! Platform-neutral core of monitrs: the data model, the rate engine, the
//! bounded history ring, process list logic, and the diagnostic engine.
//!
//! # Dependency direction
//!
//! This crate depends on **no** terminal library and **no** OS collector (§10.1).
//! It cannot know how `/proc` is parsed, and it cannot know how a process row is
//! coloured. Everything here is testable from fixtures with no live system.
//!
//! # Invariants worth stating once
//!
//! * **Unavailable is not zero.** Any metric an OS may withhold is wrapped in
//!   [`model::MetricState`], and there is no API that turns an unavailable metric
//!   into a number (§4, §26).
//! * **Stale is not current.** A retained value can only be read together with
//!   its age (§26).
//! * **The first sample of delta-based data is warming up, not zero** (§8.2).
//! * **A PID is not an identity.** [`model::ProcessIdentity`] pairs the PID with
//!   a start key so PID reuse is detectable (§26).
//! * **Monotonic time for rates and ordering**, wall-clock for display only
//!   (§8.1).
//! * Raw counters and timestamps are integral; only calculated percentages and
//!   rates use floating point, and both validate at construction (§10.4).

// §15.3: no unsafe code in the core crate, ever.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// `unwrap`/`expect` stay denied in production code — a panic here corrupts the
// terminal (§14.3). In tests they are the correct way to assert a precondition,
// so the allowance is scoped to `cfg(test)` only (§18.2: narrow allowances).
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod model;
pub mod rates;
pub mod units;

pub use model::{MetricState, ProcessIdentity, SystemSnapshot, UnavailableReason};
