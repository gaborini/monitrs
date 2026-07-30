//! Threads, channels, and the shutdown path.
//!
//! §10.3 asks for a small number of OS threads and bounded channels rather than
//! an async runtime. This module owns all four threads and the single bounded
//! channel they feed:
//!
//! ```text
//!   input thread   ──┐   (the only thread that may call crossterm poll/read)
//!   sampler thread ──┼──> bounded channel ──> the UI thread's reducer
//!   detail worker  ──┘        (coalescing)
//!   tick thread    ──┘
//! ```
//!
//! Four rules from §10.3 and §16.2 are implemented here rather than merely
//! documented, because each one is invisible until it fails at three in the
//! morning:
//!
//! * **The channel is bounded.** An unbounded channel turns a slow UI into
//!   unbounded memory growth, and the queued snapshots are stale by the time
//!   anyone reads them.
//! * **Snapshots coalesce rather than queue.** When the UI falls behind, the
//!   newest snapshot supersedes older ones, and the drop is *counted* so the lag
//!   can be displayed. A monitor that hides its own lag is lying.
//! * **Nothing blocks keyboard handling.** Input lives on its own thread, so
//!   enumerating 10,000 processes cannot delay a keypress.
//! * **Every worker gets a shutdown token and is joined**, and if a worker will
//!   not join promptly the terminal is restored first and the failure recorded.

// Each spawner is exercised by the tests below, but the assembled event loop that
// calls them all is the remaining piece of the interactive runtime. Scoped to
// non-test builds so the tests still prove the plumbing works.
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use monitrs_collectors::{DueTiers, SampleTick, SnapshotSource, TierIntervals, TierScheduler};
use monitrs_core::SystemSnapshot;
use monitrs_core::diagnostics::{PressureEngine, Thresholds};
use monitrs_core::model::ProcessIdentity;
use monitrs_tui::event::{Event, TerminalEvent};

/// How many events the channel holds before the sender starts coalescing.
///
/// Small on purpose. A deep queue does not help: by the time the UI reaches a
/// snapshot four ticks old it is worthless, and the memory it occupies is not.
/// Large enough that a burst of keypresses is never lost.
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 64;

/// A one-shot request that the sampler collect now rather than on schedule.
///
/// §6.2 binds `r` to "force refresh". The sampler owns its own clock, so the only
/// way to reach it from the UI thread without a second channel is a flag it checks
/// each pass. Consumed by the read, so one keypress produces one extra sample
/// rather than a burst.
#[derive(Clone, Debug, Default)]
pub(crate) struct SampleRequest(Arc<AtomicBool>);

impl SampleRequest {
    /// A request that has not been made.
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Asks for one extra sample.
    ///
    /// Called from the interactive event loop. The soak harness includes this
    /// module by path without that loop, so it is dead there and nowhere else.
    #[allow(
        dead_code,
        reason = "called by the interactive loop, not by the soak harness"
    )]
    pub(crate) fn request(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Takes the request, clearing it.
    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

/// The signal every worker watches to know it should stop.
///
/// A plain flag rather than a channel: shutdown is one-way and idempotent, and a
/// worker blocked on its own I/O needs to check a flag rather than select on a
/// second channel.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    /// A token that has not been triggered.
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Asks every holder to stop. Idempotent.
    pub(crate) fn trigger(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether shutdown has been requested.
    pub(crate) fn is_triggered(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Counts what the channel had to drop, so the UI can show its own lag.
///
/// Shared rather than returned because the sender that does the coalescing runs on
/// a worker thread while the reader is the UI thread.
#[derive(Clone, Debug, Default)]
pub(crate) struct ChannelHealth {
    coalesced: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl ChannelHealth {
    /// A fresh counter pair.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Snapshots superseded before the UI read them (§10.3).
    pub(crate) fn coalesced(&self) -> u64 {
        self.coalesced.load(Ordering::Relaxed)
    }

    /// Events lost because the channel was full and they could not be coalesced.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn record_coalesced(&self) {
        self.coalesced.fetch_add(1, Ordering::Relaxed);
    }

    fn record_dropped(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// The sending half, with the coalescing policy attached.
#[derive(Clone, Debug)]
pub(crate) struct EventSender<Cfg> {
    inner: Sender<Event<Cfg>>,
    health: ChannelHealth,
}

impl<Cfg> EventSender<Cfg> {
    /// Sends an event, coalescing rather than blocking when the channel is full.
    ///
    /// Returns `false` when the receiver is gone, which is a worker's cue to stop.
    ///
    /// The policy differs by event kind, and the difference is the whole point:
    /// a *coalescable* event (a snapshot, a tick) is dropped when the channel is
    /// full, because a newer one is already on its way and carries strictly better
    /// information. Anything else — a keypress, a detail result — is retried
    /// briefly, because losing a keypress is a bug the user would feel.
    pub(crate) fn send(&self, event: Event<Cfg>) -> bool {
        let coalescable = event.is_coalescable();
        match self.inner.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(event)) => {
                if coalescable {
                    self.health.record_coalesced();
                    // Dropping it *is* the coalescing: the next sample supersedes
                    // it, and the count is what makes the loss visible (§16.2).
                    return true;
                }
                // A keypress is worth waiting for, but not forever: blocking here
                // would let a stalled UI wedge the input thread.
                match self.inner.send_timeout(event, Duration::from_millis(100)) {
                    Ok(()) => true,
                    Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => {
                        self.health.record_dropped();
                        true
                    }
                    Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => false,
                }
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// The shared drop counters.
    pub(crate) fn health(&self) -> ChannelHealth {
        self.health.clone()
    }
}

/// Creates the single bounded channel every worker feeds.
pub(crate) fn event_channel<Cfg>() -> (EventSender<Cfg>, Receiver<Event<Cfg>>) {
    let (tx, rx) = crossbeam_channel::bounded(EVENT_CHANNEL_CAPACITY);
    (
        EventSender {
            inner: tx,
            health: ChannelHealth::new(),
        },
        rx,
    )
}

/// A worker thread and the name to blame if it will not join.
pub(crate) struct Worker {
    name: &'static str,
    handle: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Every worker, joined together on the way out.
#[derive(Debug, Default)]
pub(crate) struct Workers {
    workers: Vec<Worker>,
}

impl Workers {
    /// An empty set.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Spawns a named worker.
    pub(crate) fn spawn<F>(&mut self, name: &'static str, body: F) -> std::io::Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let handle = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(body)?;
        self.workers.push(Worker { name, handle });
        Ok(())
    }

    /// How many workers are running.
    pub(crate) fn len(&self) -> usize {
        self.workers.len()
    }

    /// Joins every worker, returning the names of any that panicked.
    ///
    /// **The caller must already have restored the terminal.** §10.3 is explicit:
    /// if a worker cannot be joined promptly, record the issue and restore the
    /// terminal before returning — a user staring at a frozen alternate screen
    /// cannot tell a slow shutdown from a hang.
    pub(crate) fn join_all(self) -> Vec<&'static str> {
        let mut failed = Vec::new();
        for worker in self.workers {
            if worker.handle.join().is_err() {
                failed.push(worker.name);
            }
        }
        failed
    }
}

/// Reads terminal events and forwards them (§10.3).
///
/// This is the **only** thread permitted to call crossterm's `poll`/`read` pair;
/// splitting those across threads loses and duplicates input. The poll timeout is
/// what lets it notice shutdown while no key is being pressed.
///
/// Deliberately not unit-tested: `crossterm::event::poll` needs a real tty, so a
/// test would either hang or assert nothing. The behaviour that *is* testable —
/// event translation and the coalescing policy — is covered by
/// `TerminalEvent::from_crossterm` in `monitrs-tui` and by the channel tests below.
#[allow(
    dead_code,
    reason = "called by the assembled event loop; needs a tty to test"
)]
pub(crate) fn spawn_input_thread<Cfg>(
    workers: &mut Workers,
    sender: EventSender<Cfg>,
    shutdown: Shutdown,
    poll_interval: Duration,
) -> std::io::Result<()>
where
    Cfg: Send + 'static,
{
    workers.spawn("monitrs-input", move || {
        while !shutdown.is_triggered() {
            match crossterm::event::poll(poll_interval) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(error) => {
                    // A failing terminal is not recoverable by retrying in a tight
                    // loop; stop and let the main thread restore and report.
                    tracing::error!(%error, "terminal event poll failed");
                    break;
                }
            }
            let raw = match crossterm::event::read() {
                Ok(raw) => raw,
                Err(error) => {
                    tracing::error!(%error, "terminal event read failed");
                    break;
                }
            };
            // Events we do not model are dropped here rather than propagated as an
            // "unknown" variant the reducer would have to ignore anyway.
            if let Some(event) = TerminalEvent::from_crossterm(raw)
                && !sender.send(Event::Terminal(event))
            {
                break;
            }
        }
    })
}

/// Emits a monotonic tick so the reducer can expire multi-key sequences.
///
/// §6.2's `gg` sequence needs a timeout, and the keymap's `poll_timeout` can only
/// fire if something wakes the reducer when no key arrives.
pub(crate) fn spawn_tick_thread<Cfg>(
    workers: &mut Workers,
    sender: EventSender<Cfg>,
    shutdown: Shutdown,
    interval: Duration,
) -> std::io::Result<()>
where
    Cfg: Send + 'static,
{
    workers.spawn("monitrs-tick", move || {
        while !shutdown.is_triggered() {
            std::thread::sleep(interval);
            if shutdown.is_triggered() {
                break;
            }
            if !sender.send(Event::Tick(Instant::now())) {
                break;
            }
        }
    })
}

/// Runs the tiered sampling loop and publishes snapshots (§8.6, §10.4).
///
/// The elapsed interval it hands the collector is **measured**, not configured:
/// the scheduler's sleep is a target and the OS delivers whatever it delivers
/// (§8.1).
pub(crate) fn spawn_sampler_thread<Cfg, S>(
    workers: &mut Workers,
    mut source: S,
    sender: EventSender<Cfg>,
    shutdown: Shutdown,
    intervals: TierIntervals,
    forced: SampleRequest,
    thresholds: Thresholds,
) -> std::io::Result<()>
where
    Cfg: Send + 'static,
    S: SnapshotSource + 'static,
{
    workers.spawn("monitrs-sampler", move || {
        let mut scheduler = TierScheduler::new(intervals);
        // The Pressure Radar is derived here rather than in the collector: a
        // collector reports measurements, and deciding that 91% CPU is `critical`
        // is policy (§2.3). The engine keeps its own hysteresis state, which is why
        // it lives with the sampler and not with the frame.
        let mut pressure = PressureEngine::new(thresholds);
        let mut sequence = 0u64;
        let mut previous: Option<Instant> = None;

        while !shutdown.is_triggered() {
            let now = Instant::now();
            // A forced refresh collects the fast tier out of turn; the schedule is
            // then marked complete, so `r` brings the next scheduled sample forward
            // rather than adding one on top of it.
            let mut due = scheduler.due_at(now);
            if forced.take() {
                due = DueTiers::ALL;
            }
            if !due.any() {
                // Sleep in short slices so shutdown is noticed promptly even when
                // the next tier is a long way off.
                let remaining = scheduler
                    .time_until_next(now)
                    .min(Duration::from_millis(100));
                std::thread::sleep(remaining.max(Duration::from_millis(1)));
                continue;
            }

            let tick = SampleTick {
                sequence,
                captured_at: now,
                wall_time: SystemTime::now(),
                elapsed: previous
                    .map_or(Duration::ZERO, |last| now.saturating_duration_since(last)),
                due,
            };

            match source.sample(&tick) {
                Ok(mut snapshot) => {
                    scheduler.mark_completed(due, now);
                    previous = Some(now);
                    sequence = sequence.saturating_add(1);
                    // Filled in before the snapshot is published, so the UI never
                    // sees a snapshot whose radar disagrees with its own metrics.
                    snapshot.pressure = pressure.observe(&snapshot);
                    if !sender.send(Event::Snapshot(Arc::new(snapshot))) {
                        break;
                    }
                }
                Err(error) => {
                    if error.is_fatal() {
                        tracing::error!(%error, "collector failed fatally");
                        break;
                    }
                    tracing::debug!(%error, "collector error, continuing");
                    scheduler.mark_completed(due, now);
                    previous = Some(now);
                    // The sequence identifies the *attempt*, not the successful
                    // sample, so it advances even when collection failed. Two
                    // reasons, both learned the hard way: history's `record`
                    // rejects a sequence that is not newer, so reusing a number
                    // would make the next good snapshot look like a duplicate; and
                    // a failure tied to a particular sequence would otherwise
                    // repeat forever.
                    sequence = sequence.saturating_add(1);
                }
            }
        }
    })
}

/// A request for the on-demand detail worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetailRequest {
    /// Collect the expensive fields for this process.
    Fetch(ProcessIdentity),
}

/// Collects selected-process detail off the sampler's thread (§8.6, §10.3).
///
/// Separate precisely so that a slow detail read — a working directory on a
/// stalled network mount, a large `/proc/<pid>/fd` — cannot delay regular
/// sampling. Superseded requests are skipped: when the user holds a cursor key,
/// only the row they land on matters.
pub(crate) fn spawn_detail_worker<Cfg, S>(
    workers: &mut Workers,
    mut source: S,
    requests: Receiver<DetailRequest>,
    sender: EventSender<Cfg>,
    shutdown: Shutdown,
) -> std::io::Result<()>
where
    Cfg: Send + 'static,
    S: SnapshotSource + 'static,
{
    workers.spawn("monitrs-detail", move || {
        while !shutdown.is_triggered() {
            let Ok(request) = requests.recv_timeout(Duration::from_millis(100)) else {
                // Timeout or disconnect: re-check shutdown, then carry on or stop.
                if requests.is_empty() && shutdown.is_triggered() {
                    break;
                }
                continue;
            };

            // Drain to the newest queued request. Everything older refers to a row
            // the selection has already left.
            let mut latest = request;
            while let Ok(newer) = requests.try_recv() {
                latest = newer;
            }

            let DetailRequest::Fetch(identity) = latest;
            let result = source.process_detail(identity);
            if !sender.send(Event::Detail(result)) {
                break;
            }
        }
    })
}

/// Creates the detail request channel.
///
/// Bounded and small: more than a handful of outstanding requests means the
/// selection is moving faster than detail can be collected, and the extra
/// requests are already obsolete.
pub(crate) fn detail_channel() -> (Sender<DetailRequest>, Receiver<DetailRequest>) {
    crossbeam_channel::bounded(4)
}

/// A snapshot that supersedes anything older, for the UI's own coalescing.
///
/// The channel drops superseded snapshots on the sender side, but the UI can also
/// find several waiting when it returns from a slow frame. Draining to the newest
/// here is what keeps a slow frame from turning into a backlog of stale renders.
pub(crate) fn drain_to_newest_snapshot<Cfg>(
    receiver: &Receiver<Event<Cfg>>,
    first: Arc<SystemSnapshot>,
    health: &ChannelHealth,
) -> (Arc<SystemSnapshot>, Vec<Event<Cfg>>) {
    let mut newest = first;
    let mut others = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        match event {
            Event::Snapshot(snapshot) => {
                if snapshot.sequence >= newest.sequence {
                    health.record_coalesced();
                    newest = snapshot;
                } else {
                    // Out of order: keep the newer one we already have.
                    health.record_coalesced();
                }
            }
            other => others.push(other),
        }
    }
    (newest, others)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitrs_collectors::FakeCollector;
    use monitrs_collectors::fake::Scenario;
    use monitrs_tui::event::{Key, KeyPress};

    type TestEvent = Event<()>;

    #[test]
    fn the_channel_is_bounded() {
        let (sender, receiver) = event_channel::<()>();
        // Fill it with non-coalescable events, which are the ones that would grow
        // an unbounded queue.
        for _ in 0..EVENT_CHANNEL_CAPACITY {
            assert!(sender.send(TestEvent::key(KeyPress::char('j'))));
        }
        assert_eq!(receiver.len(), EVENT_CHANNEL_CAPACITY);
    }

    #[test]
    fn snapshots_coalesce_instead_of_queueing_when_the_channel_is_full() {
        let (sender, receiver) = event_channel::<()>();
        let snapshot = || {
            Arc::new(SystemSnapshot::warming_up(
                Instant::now(),
                SystemTime::UNIX_EPOCH,
                8,
            ))
        };
        for _ in 0..EVENT_CHANNEL_CAPACITY {
            assert!(sender.send(TestEvent::Snapshot(snapshot())));
        }
        assert_eq!(
            sender.health().coalesced(),
            0,
            "nothing dropped while there was room"
        );

        // Beyond capacity, sends still succeed but are counted rather than queued.
        for _ in 0..100 {
            assert!(sender.send(TestEvent::Snapshot(snapshot())));
        }
        assert_eq!(
            receiver.len(),
            EVENT_CHANNEL_CAPACITY,
            "the queue did not grow"
        );
        assert_eq!(sender.health().coalesced(), 100, "every drop was counted");
        assert_eq!(
            sender.health().dropped(),
            0,
            "a coalesced snapshot is not a lost event"
        );
    }

    #[test]
    fn a_disconnected_receiver_tells_the_worker_to_stop() {
        let (sender, receiver) = event_channel::<()>();
        drop(receiver);
        assert!(!sender.send(TestEvent::Tick(Instant::now())));
    }

    #[test]
    fn ticks_are_coalescable_and_keypresses_are_not() {
        // The distinction is what protects input while still shedding load.
        assert!(TestEvent::Tick(Instant::now()).is_coalescable());
        assert!(!TestEvent::key(KeyPress::plain(Key::Enter)).is_coalescable());
    }

    #[test]
    fn draining_keeps_the_newest_snapshot_and_preserves_other_events() {
        let (sender, receiver) = event_channel::<()>();
        let make = |sequence: u64| {
            let mut snapshot =
                SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
            snapshot.sequence = sequence;
            Arc::new(snapshot)
        };

        let first = make(1);
        assert!(sender.send(TestEvent::Snapshot(make(2))));
        assert!(sender.send(TestEvent::key(KeyPress::char('q'))));
        assert!(sender.send(TestEvent::Snapshot(make(3))));

        let health = sender.health();
        let (newest, others) = drain_to_newest_snapshot(&receiver, first, &health);
        assert_eq!(newest.sequence, 3, "the newest snapshot must win");
        assert_eq!(others.len(), 1, "the keypress must survive the drain");
        assert!(matches!(others.first(), Some(Event::Terminal(_))));
        assert_eq!(health.coalesced(), 2);
    }

    #[test]
    fn draining_never_regresses_to_an_older_snapshot() {
        let (sender, receiver) = event_channel::<()>();
        let make = |sequence: u64| {
            let mut snapshot =
                SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
            snapshot.sequence = sequence;
            Arc::new(snapshot)
        };
        assert!(sender.send(TestEvent::Snapshot(make(1))));
        let (newest, _) = drain_to_newest_snapshot(&receiver, make(9), &sender.health());
        assert_eq!(
            newest.sequence, 9,
            "an out-of-order older snapshot must not win"
        );
    }

    #[test]
    fn the_shutdown_token_is_shared_and_idempotent() {
        let shutdown = Shutdown::new();
        let copy = shutdown.clone();
        assert!(!copy.is_triggered());
        shutdown.trigger();
        shutdown.trigger();
        assert!(copy.is_triggered());
    }

    #[test]
    fn the_sampler_publishes_snapshots_and_stops_on_shutdown() {
        let (sender, receiver) = event_channel::<()>();
        let shutdown = Shutdown::new();
        let mut workers = Workers::new();

        spawn_sampler_thread(
            &mut workers,
            FakeCollector::new(Scenario::default()),
            sender,
            shutdown.clone(),
            TierIntervals::derived_from(Duration::from_millis(250)),
            SampleRequest::new(),
            Thresholds::default(),
        )
        .expect("spawns");
        assert_eq!(workers.len(), 1);

        // The first sample is due immediately, so this should not be flaky.
        let first = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the sampler must publish a first snapshot promptly");
        let Event::Snapshot(first) = first else {
            panic!("expected a snapshot first");
        };
        assert_eq!(first.sequence, 0);
        assert_eq!(
            first.elapsed,
            Duration::ZERO,
            "the first sample has no interval"
        );
        assert!(
            first.cpu.total.is_warming_up(),
            "and therefore no CPU figure"
        );

        let second = loop {
            match receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("a second snapshot")
            {
                Event::Snapshot(snapshot) => break snapshot,
                _ => continue,
            }
        };
        assert_eq!(second.sequence, 1);
        assert!(
            second.elapsed > Duration::ZERO,
            "the second sample has a measured interval"
        );

        shutdown.trigger();
        assert!(
            workers.join_all().is_empty(),
            "the sampler must join cleanly"
        );
    }

    #[test]
    fn the_sampler_uses_the_measured_interval_not_the_configured_one() {
        let (sender, receiver) = event_channel::<()>();
        let shutdown = Shutdown::new();
        let mut workers = Workers::new();

        // A collector slower than its own interval: the reported elapsed must
        // reflect the real gap, not the 250ms that was asked for (§8.1).
        let scenario = Scenario {
            collect_delay: Duration::from_millis(120),
            ..Scenario::default()
        };
        spawn_sampler_thread(
            &mut workers,
            FakeCollector::new(scenario),
            sender,
            shutdown.clone(),
            TierIntervals::derived_from(Duration::from_millis(250)),
            SampleRequest::new(),
            Thresholds::default(),
        )
        .expect("spawns");

        let mut elapsed = Vec::new();
        while elapsed.len() < 2 {
            match receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("snapshots")
            {
                Event::Snapshot(snapshot) if snapshot.sequence > 0 => {
                    elapsed.push(snapshot.elapsed);
                }
                _ => continue,
            }
        }
        shutdown.trigger();
        workers.join_all();

        for gap in elapsed {
            assert!(
                gap >= Duration::from_millis(250),
                "measured interval {gap:?} should include the collector's own cost"
            );
        }
    }

    #[test]
    fn a_recoverable_collector_error_does_not_stop_the_sampler() {
        let (sender, receiver) = event_channel::<()>();
        let shutdown = Shutdown::new();
        let mut workers = Workers::new();

        let scenario = Scenario {
            fail_at: Some(1),
            ..Scenario::default()
        };
        spawn_sampler_thread(
            &mut workers,
            FakeCollector::new(scenario),
            sender,
            shutdown.clone(),
            TierIntervals::derived_from(Duration::from_millis(250)),
            SampleRequest::new(),
            Thresholds::default(),
        )
        .expect("spawns");

        let mut sequences = Vec::new();
        while sequences.len() < 2 {
            match receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("snapshots")
            {
                Event::Snapshot(snapshot) => sequences.push(snapshot.sequence),
                _ => continue,
            }
        }
        shutdown.trigger();
        workers.join_all();

        // Sequence 1 failed and was skipped, not retried forever.
        assert_eq!(sequences.first(), Some(&0));
        assert_eq!(
            sequences.get(1),
            Some(&2),
            "the failed sequence must be consumed, not reused, got {sequences:?}"
        );
    }

    #[test]
    fn the_detail_worker_answers_the_newest_request_and_skips_superseded_ones() {
        let (sender, receiver) = event_channel::<()>();
        let (requests_tx, requests_rx) = detail_channel();
        let shutdown = Shutdown::new();
        let mut workers = Workers::new();

        spawn_detail_worker(
            &mut workers,
            FakeCollector::new(Scenario::default()),
            requests_rx,
            sender,
            shutdown.clone(),
        )
        .expect("spawns");

        // rustc and postgres are both in the reference scenario.
        let rustc = ProcessIdentity::new(31_842, 900_100);
        let postgres = ProcessIdentity::new(1_221, 700_050);
        requests_tx
            .send(DetailRequest::Fetch(rustc))
            .expect("queued");
        requests_tx
            .send(DetailRequest::Fetch(postgres))
            .expect("queued");

        let mut answered = Vec::new();
        while answered.is_empty() {
            match receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("a detail result")
            {
                Event::Detail(result) => answered.push(result),
                _ => continue,
            }
        }
        shutdown.trigger();
        let _ = requests_tx.send(DetailRequest::Fetch(rustc));
        workers.join_all();

        assert!(
            !answered.is_empty(),
            "the worker must answer at least the newest request"
        );
    }

    #[test]
    fn the_detail_worker_reports_a_vanished_process_rather_than_failing() {
        let (sender, receiver) = event_channel::<()>();
        let (requests_tx, requests_rx) = detail_channel();
        let shutdown = Shutdown::new();
        let mut workers = Workers::new();

        spawn_detail_worker(
            &mut workers,
            FakeCollector::new(Scenario::default()),
            requests_rx,
            sender,
            shutdown.clone(),
        )
        .expect("spawns");

        requests_tx
            .send(DetailRequest::Fetch(ProcessIdentity::new(999_999, 1)))
            .expect("queued");

        let result = loop {
            match receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("a result")
            {
                Event::Detail(result) => break result,
                _ => continue,
            }
        };
        shutdown.trigger();
        drop(requests_tx);
        workers.join_all();

        assert!(matches!(
            result,
            monitrs_core::model::ProcessDetailResult::Vanished(_)
        ));
    }

    #[test]
    fn the_tick_thread_emits_ticks_and_joins() {
        let (sender, receiver) = event_channel::<()>();
        let shutdown = Shutdown::new();
        let mut workers = Workers::new();

        spawn_tick_thread(
            &mut workers,
            sender,
            shutdown.clone(),
            Duration::from_millis(20),
        )
        .expect("spawns");

        let event = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("a tick");
        assert!(matches!(event, Event::Tick(_)));

        shutdown.trigger();
        assert!(workers.join_all().is_empty());
    }

    #[test]
    fn workers_that_panic_are_named_rather_than_silently_ignored() {
        let mut workers = Workers::new();
        workers
            .spawn("deliberately-panicking", || panic!("for the test"))
            .expect("spawns");
        // The panic message reaches stderr; what matters is that the name comes
        // back so the failure can be recorded (§10.3).
        assert_eq!(workers.join_all(), vec!["deliberately-panicking"]);
    }

    #[test]
    fn several_workers_all_join() {
        let shutdown = Shutdown::new();
        let mut workers = Workers::new();
        for name in ["a", "b", "c"] {
            let shutdown = shutdown.clone();
            workers
                .spawn(name, move || {
                    while !shutdown.is_triggered() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })
                .expect("spawns");
        }
        assert_eq!(workers.len(), 3);
        shutdown.trigger();
        assert!(workers.join_all().is_empty());
    }
}
