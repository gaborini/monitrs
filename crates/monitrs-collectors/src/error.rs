//! Collector error taxonomy.
//!
//! §14.1 requires separating fatal startup errors from recoverable collector
//! errors from mere metric unavailability. That separation is structural here: a
//! metric that could not be read is **not** an error, it is a
//! [`monitrs_core::MetricState`], and nothing in this enum can represent one.
//!
//! In particular, *a vanished process during sampling is expected and is not an
//! error worth a warning log* (§14.1). It is reported as
//! [`monitrs_core::UnavailableReason::ProcessExited`] on the affected field.

use thiserror::Error;

/// Something went wrong while collecting, at a granularity coarser than one
/// metric.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CollectorError {
    /// The collector could not be constructed at all. Fatal at startup.
    ///
    /// The field is `collector` rather than the more natural `source` because
    /// `thiserror` treats a field named `source` as a nested [`std::error::Error`],
    /// which this is not.
    #[error("cannot initialise the {collector} collector: {reason}")]
    Initialisation {
        /// Which collector.
        collector: &'static str,
        /// Why.
        reason: Box<str>,
    },

    /// A whole data group failed to refresh this tick.
    ///
    /// Recoverable: the snapshot is still published, with the affected metrics
    /// marked unavailable or stale.
    #[error("refreshing {group} failed: {reason}")]
    Refresh {
        /// Which data group, e.g. `"processes"` or `"networks"`.
        group: &'static str,
        /// Why.
        reason: Box<str>,
    },

    /// Collection exceeded its time budget and was abandoned for this tick.
    ///
    /// §16.2 requires progressively reducing expensive enrichment rather than
    /// blocking, so this is a normal load-shedding outcome, not a defect.
    #[error("collecting {group} exceeded its {budget_ms}ms budget")]
    Timeout {
        /// Which data group.
        group: &'static str,
        /// The budget that was exceeded.
        budget_ms: u64,
    },

    /// The platform cannot provide this data group at all.
    ///
    /// Distinct from a permission failure, which root could fix.
    #[error("{group} is not available on this platform")]
    Unsupported {
        /// Which data group.
        group: &'static str,
    },
}

impl CollectorError {
    /// Whether the application must stop.
    ///
    /// Only initialisation failures are fatal. Everything else degrades to an
    /// unavailable metric, because a monitor that exits when one `/proc` file
    /// misbehaves is less useful than one that says so and carries on.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(self, Self::Initialisation { .. })
    }

    /// The data group this concerns, for aggregating into
    /// [`monitrs_core::model::CollectorHealth`].
    #[must_use]
    pub const fn group(&self) -> &'static str {
        match self {
            Self::Initialisation { collector, .. } => collector,
            Self::Refresh { group, .. }
            | Self::Timeout { group, .. }
            | Self::Unsupported { group } => group,
        }
    }

    /// Whether this is worth telling the user about.
    ///
    /// An unsupported data group is a permanent, already-visible fact — the
    /// Inspect screen shows it as a capability — so repeating it as a collector
    /// issue every tick would be noise (§9.2's rule against logging one error per
    /// expected event).
    #[must_use]
    pub const fn is_noteworthy(&self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_initialisation_failures_are_fatal() {
        assert!(
            CollectorError::Initialisation {
                collector: "sysinfo",
                reason: "no memory".into()
            }
            .is_fatal()
        );
        assert!(
            !CollectorError::Refresh {
                group: "processes",
                reason: "eperm".into()
            }
            .is_fatal()
        );
        assert!(
            !CollectorError::Timeout {
                group: "filesystems",
                budget_ms: 200
            }
            .is_fatal()
        );
        assert!(!CollectorError::Unsupported { group: "psi" }.is_fatal());
    }

    #[test]
    fn an_unsupported_group_is_not_reported_as_a_recurring_issue() {
        // It is already visible as a capability; repeating it every tick is noise.
        assert!(!CollectorError::Unsupported { group: "psi" }.is_noteworthy());
        assert!(
            CollectorError::Refresh {
                group: "disks",
                reason: "io".into()
            }
            .is_noteworthy()
        );
    }

    #[test]
    fn every_variant_reports_the_group_it_concerns() {
        assert_eq!(CollectorError::Unsupported { group: "psi" }.group(), "psi");
        assert_eq!(
            CollectorError::Timeout {
                group: "filesystems",
                budget_ms: 1
            }
            .group(),
            "filesystems"
        );
        assert_eq!(
            CollectorError::Initialisation {
                collector: "sysinfo",
                reason: "x".into()
            }
            .group(),
            "sysinfo"
        );
    }

    #[test]
    fn messages_name_the_group_so_a_log_line_is_actionable() {
        let error = CollectorError::Refresh {
            group: "networks",
            reason: "ENODEV".into(),
        };
        let message = error.to_string();
        assert!(message.contains("networks"), "{message}");
        assert!(message.contains("ENODEV"), "{message}");
    }
}
