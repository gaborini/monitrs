//! Pins the contract of `interactive.rs`'s effect executor now that [`Effect`]
//! is `#[non_exhaustive]` (Task 9b).
//!
//! `#[non_exhaustive]` on `Effect` is what let 1.1.0's pressure event log and
//! panel zoom add effects without forcing a `2.0.0`, but it cost the compiler's
//! proof that `interactive::execute`'s match on `Effect` is complete: a variant
//! this crate cannot see might exist on some future `monitrs-tui`, so the match
//! now ends in a wildcard arm rather than an exhaustiveness check. That arm logs
//! at error level and returns `Flow::Continue` (§14.3 forbids panicking here)
//! rather than doing nothing silently — a feature that appears to work and
//! quietly has no effect is the failure mode this whole task exists to avoid.
//!
//! # Why this test cannot construct the case the wildcard exists for
//!
//! The wildcard is reachable only by an `Effect` variant this build does not
//! know about, and there is no such value to construct: every variant `Effect`
//! has today is matched by an explicit arm in `execute` (this is the desired
//! state — see `CONTRIBUTING.md`). So rather than exercising the wildcard arm
//! itself, this test pins its *contract* by shape: every effect the executor
//! recognises, other than [`Effect::Shutdown`], returns `Flow::Continue` — which
//! is exactly what the wildcard arm also returns for anything it does not
//! recognise. `Effect::Shutdown` is the one call that must reach `Flow::Stop`,
//! and asserting that boundary is what makes "everything else continues" mean
//! something rather than being vacuously true.
//!
//! [`Effect::SignalProcess`] and [`Effect::ReniceProcess`] are deliberately
//! excluded from the sweep below: they act on a real process
//! ([`Effect::touches_a_process`] is what the rest of the codebase uses to draw
//! exactly this line), and this test has no process it is safe to act on.
//! Nothing about `#[non_exhaustive]` changes how those two are executed, so they
//! are outside this task's guard and are already covered by
//! `crates/monitrs/tests/wiring.rs` and `interactive.rs`'s own unit tests.
//!
//! # Why the module source is included rather than imported
//!
//! `monitrs` is a binary crate, so there is no library target to link against,
//! and `execute`, `Flow` and `EffectContext` are deliberately `pub(crate)` —
//! visible within the binary crate, not exported outside it. `#[path]` puts the
//! real modules into this test binary, which is the only way to drive them
//! without widening their visibility further, the same trade `integration.rs`
//! and `soak.rs` already make for `runtime.rs`, `config.rs` and `export.rs`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              failure must name the line that broke"
)]

use core::time::Duration;
use std::path::PathBuf;

use monitrs_core::diagnostics::Thresholds;
use monitrs_core::model::ProcessIdentity;
use monitrs_tui::action::Effect;
use monitrs_tui::app::AppState;

#[path = "../src/cli.rs"]
mod cli;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/export.rs"]
mod export;
// `runtime` records collector timings through `logging`, so the real module has
// to come along, as `integration.rs` explains at its own inclusion of it.
#[path = "../src/logging.rs"]
mod logging;
#[path = "../src/runtime.rs"]
mod runtime;
// `interactive::execute`'s `Effect::SignalProcess` arm names `signals::deliver`,
// so the module has to compile even though this file never exercises that arm.
#[path = "../src/signals.rs"]
mod signals;
// Only `execute`, `Flow` and `EffectContext` are exercised below. `run` is the
// assembled application — terminal, workers, the event loop — and has no place
// in a test that spawns none of that; `draw`, `draw_attribution`,
// `presentation_for` and `draw_overlay` are rendering, already covered by
// `monitrs-tui`'s own snapshot suites. Unused from this file's narrow
// perspective, not unused from the binary's.
#[allow(dead_code)]
#[path = "../src/interactive.rs"]
mod interactive;

use interactive::{EffectContext, Flow, execute};

/// A context built the way `interactive::run` builds one, minus the workers.
///
/// `config_path: None` on the state (via [`AppState::default`]) keeps
/// `Effect::ReloadConfig` on its immediate, file-free refusal path, and no live
/// snapshot keeps `Effect::ExportSnapshot` on its "nothing to export yet" path —
/// both deliberately, so this test touches no real file.
#[allow(
    clippy::type_complexity,
    reason = "a plain tuple of test fixtures reads more clearly here than a named struct \
              that exists for one helper"
)]
fn context() -> (
    EffectContext,
    crossbeam_channel::Receiver<runtime::DetailRequest>,
    runtime::Shutdown,
) {
    let (detail_tx, detail_rx) = runtime::detail_channel();
    let (sender, _events) = runtime::event_channel::<Result<Box<config::Config>, String>>();
    // A separate handle to the same flag `ctx` gets, since `EffectContext`'s
    // fields are private and a test outside `interactive.rs` has no other way
    // to observe what `Effect::Shutdown` did to it.
    let shutdown = runtime::Shutdown::new();
    let ctx = EffectContext::new(
        detail_tx,
        runtime::SampleRequest::new(),
        runtime::SensorInterest::new(),
        shutdown.clone(),
        config::Config::default(),
        sender,
        runtime::SamplingControl::new(Duration::from_secs(1), Thresholds::default()),
        false,
        false,
    );
    (ctx, detail_rx, shutdown)
}

fn identity() -> ProcessIdentity {
    ProcessIdentity::new(31_842, 900_100)
}

/// Every effect the executor recognises, except [`Effect::Shutdown`] and the
/// two that touch a real process, must continue the loop.
///
/// This is the arm's contract pinned by shape (§9b Step 4): the wildcard arm a
/// non-exhaustive `Effect` now requires returns `Flow::Continue`, and this
/// proves that is not a special case — it is what every recognised effect
/// without a process to act on already does.
#[test]
fn every_effect_without_a_process_to_act_on_continues_the_loop() {
    let (mut ctx, detail_rx, _shutdown) = context();
    let mut state = AppState::default();

    let sweep = [
        Effect::None,
        Effect::RequestRedraw,
        Effect::RequestSample,
        Effect::SetSensorInterest(true),
        Effect::SetSensorInterest(false),
        Effect::FetchProcessDetail(identity()),
        Effect::RingBell,
        Effect::ReloadConfig,
        Effect::ExportSnapshot(PathBuf::from(
            "/nonexistent/effect-executor-test/should-not-be-written.json",
        )),
    ];

    for effect in &sweep {
        assert_eq!(
            execute(effect, &mut state, &mut ctx),
            Flow::Continue,
            "{effect:?} must continue the loop"
        );
    }

    // `FetchProcessDetail` really did queue the request rather than doing
    // nothing: the "no side effect" framing above is about *acting on a
    // process*, not about doing literally nothing.
    assert_eq!(
        detail_rx.try_recv(),
        Ok(runtime::DetailRequest::Fetch(identity())),
        "the detail worker's queue must have received the request"
    );

    // `ExportSnapshot` took the "nothing to export yet" branch rather than
    // writing: there is no live snapshot in a freshly built `AppState`.
    assert!(
        !PathBuf::from("/nonexistent/effect-executor-test/should-not-be-written.json").exists(),
        "§10.5: the reducer's effect must not have written anything without a live sample"
    );
}

/// The one effect that must *not* continue the loop.
///
/// Asserted separately so the sweep above is not vacuously true: if `execute`
/// returned `Flow::Continue` unconditionally, the sweep would still pass.
#[test]
fn shutdown_is_the_only_effect_that_stops_the_loop() {
    let (mut ctx, _detail_rx, shutdown) = context();
    let mut state = AppState::default();

    assert_eq!(execute(&Effect::Shutdown, &mut state, &mut ctx), Flow::Stop);
    assert!(shutdown.is_triggered());
}
