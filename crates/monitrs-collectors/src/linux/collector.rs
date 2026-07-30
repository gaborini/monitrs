//! The Linux [`SnapshotSource`]: the baseline collector plus native enrichment.
//!
//! This file is deliberately thin, and it is the only part of the Linux layer that is
//! gated to Linux. Everything it does is glue:
//!
//! 1. let [`CommonCollector`] produce a complete snapshot;
//! 2. read the `/proc` and `/sys` sources the due tiers call for;
//! 3. hand both to [`LinuxEnrichment::apply`].
//!
//! Keeping the glue this small is what makes the rest of the module testable on a
//! machine with no `/proc` (§17.2): the parsers, the reading layer, and the
//! enrichment all run from fixtures, and only these few dozen lines depend on being
//! on Linux.

use monitrs_core::SystemSnapshot;
use monitrs_core::model::{
    CapabilitySnapshot, CapabilityState, MetricState, ProcessDetailResult, ProcessIdentity, Tier,
};

use crate::common::CommonCollector;
use crate::error::CollectorError;
use crate::linux::cgroup::parse_pid_cgroup;
use crate::linux::enrich::LinuxEnrichment;
use crate::linux::process::parse_pid_stat;
use crate::linux::read::{MAX_ENRICHED_PROCESSES, ProcRoot, SourceRequest, collect_sources};
use crate::linux::signal::{KillSink, LinuxSignal, SignalDecision, SignalError, signal_process};
use crate::source::{SampleTick, SnapshotSource};

/// The Linux collector.
///
/// Holds the baseline collector and every enrichment baseline for the life of the
/// process; recreating it per tick would destroy every delta (§9.1, §26).
#[derive(Debug)]
pub struct LinuxCollector {
    common: CommonCollector,
    root: ProcRoot,
    enrichment: LinuxEnrichment,
}

impl LinuxCollector {
    /// Builds a collector against the live `/proc` and `/sys`.
    pub fn new() -> Result<Self, CollectorError> {
        Ok(Self {
            common: CommonCollector::new()?,
            root: ProcRoot::live(),
            enrichment: LinuxEnrichment::new(),
        })
    }

    /// Builds a collector against an arbitrary root, for a sandbox that mounts
    /// `/proc` elsewhere.
    pub fn with_root(root: ProcRoot) -> Result<Self, CollectorError> {
        Ok(Self {
            common: CommonCollector::new()?,
            root,
            enrichment: LinuxEnrichment::new(),
        })
    }

    /// The enrichment state, for the diagnostics and cgroup figures §7.5 renders.
    #[must_use]
    pub const fn enrichment(&self) -> &LinuxEnrichment {
        &self.enrichment
    }

    /// Revalidates `identity` and, only if it still matches, delivers `signal`.
    ///
    /// The `/proc/<pid>/stat` read happens *here*, one statement before delivery, which
    /// is what §9.2's "revalidate immediately before signalling" means. Nothing else
    /// on this type can reach [`KillSink`].
    pub fn signal(
        &mut self,
        identity: ProcessIdentity,
        signal: LinuxSignal,
    ) -> Result<SignalDecision, SignalError> {
        let fresh = self.root.read_pid(identity.pid, "stat");
        let mut sink = KillSink;
        signal_process(
            &mut sink,
            identity,
            signal,
            // `ReadFailure` is `Copy`, so the borrow of the buffer does not outlive
            // this call while the failure itself travels by value.
            fresh.as_deref().map_err(|failure| *failure),
        )
    }
}

impl SnapshotSource for LinuxCollector {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn capabilities(&self) -> CapabilitySnapshot {
        // The baseline's view, enriched with what only this layer can answer. The
        // per-sample capabilities on the snapshot itself are more precise, because
        // they record what the current tick actually managed to read (§4).
        let mut capabilities = self.common.capabilities();
        capabilities.kernel_threads = CapabilityState::Available;
        capabilities.process_signals = CapabilityState::Available;
        capabilities.renice = crate::renice::capability_state();
        capabilities
    }

    fn sample(&mut self, tick: &SampleTick) -> Result<SystemSnapshot, CollectorError> {
        let mut snapshot = self.common.sample(tick)?;

        // One directory level, capped, never recursive (§9.2).
        let pids = if tick.due.contains(Tier::Fast) {
            self.root
                .list_pids(MAX_ENRICHED_PROCESSES)
                .map(|(pids, _truncated)| pids)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let sources = collect_sources(
            &self.root,
            &SourceRequest {
                tiers: tick.due,
                pids,
                // §8.6 puts cgroup metadata on the slow tier: a container's cgroup
                // does not change while it runs, and reading it per process per tick
                // would be the most expensive thing in the pass.
                include_process_cgroups: tick.due.contains(Tier::Slow),
            },
        );
        self.enrichment.apply(&mut snapshot, &sources, tick);
        // The baseline declares renice unsupported because the `sysinfo` layer has no
        // write path; this layer does (`crate::renice`). It has to be set on the
        // snapshot as well as in `capabilities`, because the snapshot's copy is what
        // the UI gates the `R` key on (§4, §6.2).
        snapshot.capabilities.renice = crate::renice::capability_state();
        Ok(snapshot)
    }

    fn process_detail(&mut self, identity: ProcessIdentity) -> ProcessDetailResult {
        // The baseline resolves ancestry, children, and the working directory. Reuse
        // it rather than re-deriving, then add what only `/proc` knows.
        let result = self.common.process_detail(identity);
        let ProcessDetailResult::Loaded(mut detail) = result else {
            return result;
        };

        // Re-read `stat` to confirm the identity has not changed under us. §14.1: a
        // process that exited between the two reads is expected, not an error.
        match self.root.read_pid(identity.pid, "stat") {
            Ok(bytes) => match parse_pid_stat(&bytes) {
                Ok(stat) if stat.identity() == identity => {
                    detail.nice = match i32::try_from(stat.nice) {
                        Ok(nice) => MetricState::Available(nice),
                        // A niceness outside `i32` means the field was not niceness.
                        Err(_) => MetricState::Unsupported,
                    };
                }
                Ok(stat) => {
                    return ProcessDetailResult::Reused {
                        requested: identity,
                        found: stat.identity(),
                    };
                }
                Err(_) => {}
            },
            Err(crate::linux::read::ReadFailure::Missing) => {
                return ProcessDetailResult::Vanished(identity);
            }
            Err(_) => {}
        }

        if let Ok(bytes) = self.root.read_pid(identity.pid, "cgroup")
            && let Ok(membership) = parse_pid_cgroup(&bytes)
        {
            if let Some(path) = membership.primary_path() {
                detail.cgroup = MetricState::Available(path.into());
            }
            detail.container = match membership.container() {
                Some(container) => MetricState::Available(container.label().into()),
                // Not being in a container is a fact about this process rather than a
                // failed read, so it is `Unsupported` and not a temporary failure.
                None => MetricState::Unsupported,
            };
        }

        // §8.6 puts descriptor counting in the on-demand tier precisely because it is
        // a directory read; it is capped, and a denial is a metric state (§9.2).
        detail.open_files = match self.root.count_open_files(identity.pid) {
            // A capped count is a floor rather than a total. `MetricState<u32>` has no
            // way to say "at least", and a floor of 65 536 descriptors is a far more
            // useful thing to show than no number at all, so it is reported as
            // measured — the cap is documented on `MAX_COUNTED_FDS`.
            Ok((count, _capped)) => MetricState::Available(count),
            Err(failure) => failure.process_state(),
        };

        ProcessDetailResult::Loaded(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime};

    // §17.6 platform smoke tests: these read the live system, so they are
    // `#[ignore]`d to keep `cargo test` hermetic. CI runs them with `-- --ignored` on
    // a real Linux runner.

    #[test]
    #[ignore = "platform smoke test: reads the live /proc"]
    fn smoke_the_linux_collector_enriches_the_baseline_snapshot() {
        let mut collector = LinuxCollector::new().expect("constructs on Linux");
        let mut tick = SampleTick::first(Instant::now(), SystemTime::now());
        let first = collector.sample(&tick).expect("first sample");
        assert!(
            first.cpu.total.is_warming_up(),
            "the first sample must not report a number"
        );

        std::thread::sleep(std::time::Duration::from_millis(300));
        tick = tick.advance(
            Instant::now(),
            SystemTime::now(),
            crate::tier::DueTiers::ALL,
        );
        let second = collector.sample(&tick).expect("second sample");

        let cpu = second
            .cpu
            .total
            .fresh()
            .expect("measured on the second pass");
        assert!((0.0..=100.0).contains(&cpu.busy.value()));
        assert!(
            cpu.breakdown.fresh().is_some(),
            "/proc/stat gives a breakdown the baseline cannot"
        );
        assert_eq!(
            second.memory.semantics,
            monitrs_core::model::MemorySemantics::LinuxMemAvailable
        );

        let me = std::process::id();
        let process = second.process_by_pid(me).expect("our own process");
        assert!(
            process.identity.start_key > 1_000,
            "the start key must be clock ticks, not whole seconds: {}",
            process.identity.start_key
        );
        assert!(process.io.read_total_bytes.fresh().is_some());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live /proc"]
    fn smoke_signalling_a_dead_pid_is_refused_rather_than_delivered() {
        let mut collector = LinuxCollector::new().expect("constructs on Linux");
        // PID 0 is the scheduler and is never a signallable user process.
        let phantom = ProcessIdentity::new(0, 0);
        let decision = collector
            .signal(phantom, LinuxSignal::Term)
            .expect("no delivery attempted");
        assert!(decision.deliverable().is_none(), "got {decision:?}");
    }
}
