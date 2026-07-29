//! Platform-neutral core of monitrs: the data model, the rate engine, the
//! bounded history ring, process list logic, and the diagnostic engine.
//!
//! This crate depends on no terminal library and no OS collector (§10.1).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]
