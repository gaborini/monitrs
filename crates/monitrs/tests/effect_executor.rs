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
//! # `Flow::Continue` alone is a weak guard, and the sweep below says so
//!
//! Because the wildcard *also* returns `Flow::Continue`, an assertion that only
//! checks the return value cannot tell "the named arm ran" apart from "the arm
//! was deleted and this fell through to the wildcard." For `Effect::RequestSample`
//! and `Effect::SetSensorInterest`, a second, independent observable exists
//! ([`runtime::SampleRequest::take`], [`runtime::SensorInterest::get`]) and is
//! asserted below; for `Effect::FetchProcessDetail` the detail channel is that
//! observable already. For `Effect::None`, `Effect::RequestRedraw`,
//! `Effect::RingBell`, `Effect::ReloadConfig` and `Effect::ExportSnapshot` no
//! equally cheap, local, non-side-effecting observable exists — asserting
//! `Flow::Continue` for those five is smoke (it would not notice their arm
//! being deleted), not a guard, and the test below says so at the point it
//! checks them rather than leaving a reader to assume otherwise.
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

/// The fixtures `interactive::run` builds before the loop, minus the workers.
///
/// `config_path: None` on the state (via [`AppState::default`]) keeps
/// `Effect::ReloadConfig` on its immediate, file-free refusal path, and no live
/// snapshot keeps `Effect::ExportSnapshot` on its "nothing to export yet" path —
/// both deliberately, so this test touches no real file.
///
/// A named struct rather than a tuple: `EffectContext`'s own fields are
/// private, so the only way this test can observe what an effect did to
/// `forced` or `sensor_interest` — rather than only what `execute` returned —
/// is to keep a second handle to the same underlying flag, exactly as it
/// already does for `shutdown`. Once there were three of those handles
/// alongside the context and the channel, a tuple's positions stopped being
/// easy to read at the call site.
struct Fixture {
    ctx: EffectContext,
    detail_rx: crossbeam_channel::Receiver<runtime::DetailRequest>,
    shutdown: runtime::Shutdown,
    /// A second handle to the flag `Effect::RequestSample` sets, so the test
    /// can tell "the arm ran" apart from "this fell through to the wildcard,
    /// which also returns `Flow::Continue`."
    forced: runtime::SampleRequest,
    /// A second handle to the flag `Effect::SetSensorInterest` sets, for the
    /// same reason.
    sensor_interest: runtime::SensorInterest,
}

impl Fixture {
    fn new() -> Self {
        let (detail_tx, detail_rx) = runtime::detail_channel();
        let (sender, _events) = runtime::event_channel::<Result<Box<config::Config>, String>>();
        let shutdown = runtime::Shutdown::new();
        let forced = runtime::SampleRequest::new();
        let sensor_interest = runtime::SensorInterest::new();
        let ctx = EffectContext::new(
            detail_tx,
            forced.clone(),
            sensor_interest.clone(),
            shutdown.clone(),
            config::Config::default(),
            sender,
            runtime::SamplingControl::new(Duration::from_secs(1), Thresholds::default()),
            false,
            false,
        );
        Self {
            ctx,
            detail_rx,
            shutdown,
            forced,
            sensor_interest,
        }
    }
}

fn identity() -> ProcessIdentity {
    ProcessIdentity::new(31_842, 900_100)
}

/// Every effect the executor recognises, except [`Effect::Shutdown`] and the
/// two that touch a real process, must continue the loop — and, where a cheap
/// independent observable exists, must actually have done the thing its arm is
/// responsible for, not merely have returned `Flow::Continue` (see the module
/// doc comment: the wildcard returns that too).
#[test]
fn every_effect_without_a_process_to_act_on_continues_the_loop() {
    let mut fx = Fixture::new();
    let mut state = AppState::default();

    // --- smoke: `Flow::Continue` is all that is checked here ---------------
    //
    // No arm below has a side effect this test can observe more cheaply than
    // "the loop kept going," so none of these five would notice its arm being
    // deleted and falling through to the wildcard. `RingBell` writes to the
    // real terminal bell; `ReloadConfig` and `ExportSnapshot` are exercised
    // against their real branches (a missing config path, a missing live
    // snapshot) in `interactive.rs`'s own unit tests and in
    // `crates/monitrs/tests/integration.rs`. This loop only proves they do not
    // panic and do not stop the loop.
    for effect in [
        Effect::None,
        Effect::RequestRedraw,
        Effect::RingBell,
        Effect::ReloadConfig,
        Effect::ExportSnapshot(PathBuf::from(
            "/nonexistent/effect-executor-test/should-not-be-written.json",
        )),
    ] {
        assert_eq!(
            execute(&effect, &mut state, &mut fx.ctx),
            Flow::Continue,
            "{effect:?} must continue the loop"
        );
    }
    // `ExportSnapshot` took the "nothing to export yet" branch rather than
    // writing: there is no live snapshot in a freshly built `AppState`.
    assert!(
        !PathBuf::from("/nonexistent/effect-executor-test/should-not-be-written.json").exists(),
        "§10.5: the reducer's effect must not have written anything without a live sample"
    );

    // --- guards: the arm's side effect is checked independently of `Flow` --

    assert_eq!(
        execute(&Effect::RequestSample, &mut state, &mut fx.ctx),
        Flow::Continue
    );
    assert!(
        fx.forced.take(),
        "Effect::RequestSample must set the forced-sample flag, not just continue"
    );

    assert_eq!(
        execute(&Effect::SetSensorInterest(true), &mut state, &mut fx.ctx),
        Flow::Continue
    );
    assert!(
        fx.sensor_interest.get(),
        "Effect::SetSensorInterest(true) must set the sensor-interest flag, not just continue"
    );

    assert_eq!(
        execute(&Effect::SetSensorInterest(false), &mut state, &mut fx.ctx),
        Flow::Continue
    );
    assert!(
        !fx.sensor_interest.get(),
        "Effect::SetSensorInterest(false) must clear the sensor-interest flag, not just continue"
    );

    assert_eq!(
        execute(
            &Effect::FetchProcessDetail(identity()),
            &mut state,
            &mut fx.ctx
        ),
        Flow::Continue
    );
    // `FetchProcessDetail` really did queue the request rather than doing
    // nothing: the wildcard arm never touches this channel, so a deleted
    // `FetchProcessDetail` arm would leave it empty.
    assert_eq!(
        fx.detail_rx.try_recv(),
        Ok(runtime::DetailRequest::Fetch(identity())),
        "the detail worker's queue must have received the request"
    );
}

/// The one effect that must *not* continue the loop.
///
/// Asserted separately so the sweep above is not vacuously true: if `execute`
/// returned `Flow::Continue` unconditionally, the sweep would still pass.
#[test]
fn shutdown_is_the_only_effect_that_stops_the_loop() {
    let mut fx = Fixture::new();
    let mut state = AppState::default();

    assert_eq!(
        execute(&Effect::Shutdown, &mut state, &mut fx.ctx),
        Flow::Stop
    );
    assert!(fx.shutdown.is_triggered());
}
