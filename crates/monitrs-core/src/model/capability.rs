//! Per-metric platform capability.
//!
//! §4: *do not represent platform support as one global boolean.* This snapshot
//! is what the Inspect screen renders under "unavailable metrics and why" (§7.5)
//! and what the layout engine consults before reserving space for an optional
//! panel.
//!
//! Capabilities are a fixed struct rather than a map so that a snapshot costs no
//! allocation and adding a capability is a compile error at every match site.

/// Whether one capability is usable on this system.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum CapabilityState {
    /// Present and readable.
    Available,
    /// This platform or kernel does not provide it.
    Unsupported,
    /// Present but the OS refuses the read at our privilege level.
    ///
    /// Distinct from `Unsupported` because §4 requires a help hint suggesting
    /// what elevated privileges would provide.
    PermissionDenied,
    /// Not probed yet.
    #[default]
    Unknown,
}

impl CapabilityState {
    /// Lower-case label for the Inspect screen.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unsupported => "unsupported",
            Self::PermissionDenied => "permission denied",
            Self::Unknown => "not probed",
        }
    }

    /// A redundant non-color cue (§5.2).
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Available => '+',
            Self::Unsupported => '-',
            Self::PermissionDenied => '!',
            Self::Unknown => '?',
        }
    }

    /// Whether elevated privileges would plausibly help.
    ///
    /// Drives the help hint §4 requires. Note that §15.1 forbids monitrs from
    /// escalating on its own; this only informs the user.
    #[must_use]
    pub const fn privileges_might_help(self) -> bool {
        matches!(self, Self::PermissionDenied)
    }
}

/// Every capability the UI branches on.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CapabilitySnapshot {
    /// Per-process read/write byte counters.
    pub per_process_io: CapabilityState,
    /// Per-process thread counts.
    pub per_process_threads: CapabilityState,
    /// Per-process open file descriptor counts.
    pub per_process_open_files: CapabilityState,
    /// Per-process socket counts.
    pub per_process_sockets: CapabilityState,
    /// Per-process working directory.
    pub per_process_working_directory: CapabilityState,
    /// Per-logical-CPU utilization.
    pub per_core_cpu: CapabilityState,
    /// The user/system/idle CPU time split.
    pub cpu_breakdown: CapabilityState,
    /// Load averages.
    pub load_average: CapabilityState,
    /// Swap-in and swap-out rates, as opposed to swap capacity.
    pub swap_activity: CapabilityState,
    /// Block-device throughput counters.
    pub disk_io: CapabilityState,
    /// Block-device busy percentage (§7.3).
    pub disk_busy: CapabilityState,
    /// Filesystem capacity.
    pub filesystem_capacity: CapabilityState,
    /// Interface byte and packet counters.
    pub network_counters: CapabilityState,
    /// Negotiated link speed, without which no utilization is shown (§7.4).
    pub network_link_speed: CapabilityState,
    /// Interface error and drop counters.
    pub network_errors: CapabilityState,
    /// Temperature sensors.
    pub temperatures: CapabilityState,
    /// Battery.
    pub battery: CapabilityState,
    /// Linux `/proc/pressure/*`.
    pub linux_psi: CapabilityState,
    /// cgroup limits, exposed separately from host totals (§9.2).
    pub cgroup_limits: CapabilityState,
    /// Whether kernel threads are distinguishable, so they can be hidden (§7.2).
    pub kernel_threads: CapabilityState,
    /// Whether signals can be sent at all.
    pub process_signals: CapabilityState,
    /// Whether renice is available (§6.2).
    pub renice: CapabilityState,
}

impl CapabilitySnapshot {
    /// The number of capabilities tracked.
    pub const COUNT: usize = 22;

    /// Every capability paired with its display label, for the Inspect screen.
    ///
    /// The order is stable so the panel does not reshuffle between frames.
    #[must_use]
    pub fn entries(&self) -> [(&'static str, CapabilityState); Self::COUNT] {
        [
            ("process I/O", self.per_process_io),
            ("process threads", self.per_process_threads),
            ("process open files", self.per_process_open_files),
            ("process sockets", self.per_process_sockets),
            (
                "process working directory",
                self.per_process_working_directory,
            ),
            ("per-core CPU", self.per_core_cpu),
            ("CPU time breakdown", self.cpu_breakdown),
            ("load average", self.load_average),
            ("swap activity", self.swap_activity),
            ("disk I/O", self.disk_io),
            ("disk busy", self.disk_busy),
            ("filesystem capacity", self.filesystem_capacity),
            ("network counters", self.network_counters),
            ("network link speed", self.network_link_speed),
            ("network errors", self.network_errors),
            ("temperatures", self.temperatures),
            ("battery", self.battery),
            ("Linux PSI", self.linux_psi),
            ("cgroup limits", self.cgroup_limits),
            ("kernel threads", self.kernel_threads),
            ("process signals", self.process_signals),
            ("renice", self.renice),
        ]
    }

    /// Capabilities that are missing, with the reason, for the diagnostics
    /// subsection §7.5 requires.
    #[must_use]
    pub fn unavailable(&self) -> Vec<(&'static str, CapabilityState)> {
        self.entries()
            .into_iter()
            .filter(|(_, state)| *state != CapabilityState::Available)
            .collect()
    }

    /// Whether any capability is denied rather than merely absent, so the UI can
    /// show one privilege hint instead of one per metric.
    #[must_use]
    pub fn any_permission_denied(&self) -> bool {
        self.entries()
            .into_iter()
            .any(|(_, state)| state.privileges_might_help())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_covers_every_declared_capability() {
        let capabilities = CapabilitySnapshot::default();
        assert_eq!(capabilities.entries().len(), CapabilitySnapshot::COUNT);
    }

    #[test]
    fn entry_labels_are_unique_and_ascii() {
        let entries = CapabilitySnapshot::default().entries();
        let mut labels: Vec<&str> = entries.iter().map(|(label, _)| *label).collect();
        assert!(labels.iter().all(|label| label.is_ascii()));
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(
            labels.len(),
            CapabilitySnapshot::COUNT,
            "duplicate capability label"
        );
    }

    #[test]
    fn an_unprobed_snapshot_reports_everything_as_unavailable() {
        let capabilities = CapabilitySnapshot::default();
        assert_eq!(capabilities.unavailable().len(), CapabilitySnapshot::COUNT);
        assert!(
            !capabilities.any_permission_denied(),
            "unknown is not denied"
        );
    }

    #[test]
    fn permission_denied_is_distinct_from_unsupported() {
        let capabilities = CapabilitySnapshot {
            per_process_io: CapabilityState::PermissionDenied,
            linux_psi: CapabilityState::Unsupported,
            ..CapabilitySnapshot::default()
        };

        assert!(capabilities.any_permission_denied());
        assert!(CapabilityState::PermissionDenied.privileges_might_help());
        assert!(
            !CapabilityState::Unsupported.privileges_might_help(),
            "root cannot conjure a kernel feature that does not exist"
        );
    }

    #[test]
    fn available_capabilities_drop_out_of_the_unavailable_list() {
        let capabilities = CapabilitySnapshot {
            per_process_io: CapabilityState::Available,
            ..CapabilitySnapshot::default()
        };
        let unavailable = capabilities.unavailable();
        assert_eq!(unavailable.len(), CapabilitySnapshot::COUNT - 1);
        assert!(!unavailable.iter().any(|(label, _)| *label == "process I/O"));
    }

    #[test]
    fn state_symbols_are_distinguishable_without_color() {
        let mut symbols: Vec<char> = [
            CapabilityState::Available,
            CapabilityState::Unsupported,
            CapabilityState::PermissionDenied,
            CapabilityState::Unknown,
        ]
        .iter()
        .map(|state| state.symbol())
        .collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), 4);
    }
}
