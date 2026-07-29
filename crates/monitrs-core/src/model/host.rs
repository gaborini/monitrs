//! System identity and environment.

use core::time::Duration;
use std::time::SystemTime;

use crate::model::{Confidence, MetricState};

/// Whether we appear to be running on hardware, in a VM, or in a container.
///
/// §7.5 requires this to be *clearly labelled heuristic*, which is why
/// [`HostEnvironment`] carries both the evidence and a [`Confidence`] and why
/// there is no `BareMetal` variant — absence of container and VM evidence is not
/// proof of bare metal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EnvironmentKind {
    /// No container or virtualization evidence was found.
    #[default]
    NoEvidenceFound,
    /// Container evidence was found, e.g. a Docker or Kubernetes cgroup path.
    Container,
    /// Virtualization evidence was found, e.g. a hypervisor DMI string.
    VirtualMachine,
}

impl EnvironmentKind {
    /// Lower-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoEvidenceFound => "no container/VM evidence",
            Self::Container => "container",
            Self::VirtualMachine => "virtual machine",
        }
    }
}

/// A heuristic environment classification together with its evidence.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct HostEnvironment {
    /// What we think this is.
    pub kind: EnvironmentKind,
    /// What led to that conclusion, e.g. `"/proc/1/cgroup names docker"`.
    ///
    /// Rendered next to the classification so the user can judge it themselves.
    pub evidence: Box<str>,
    /// How much the evidence supports the conclusion.
    pub confidence: Confidence,
}

/// System identity, mostly from the slow sampling tier (§8.6).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct HostSnapshot {
    /// Host name.
    pub hostname: MetricState<Box<str>>,
    /// OS name, e.g. `macOS` or `Debian GNU/Linux`.
    pub os_name: MetricState<Box<str>>,
    /// OS version.
    pub os_version: MetricState<Box<str>>,
    /// Kernel version.
    pub kernel_version: MetricState<Box<str>>,
    /// Target architecture. Known at compile time, so never unavailable.
    pub arch: &'static str,
    /// CPU model string.
    pub cpu_brand: MetricState<Box<str>>,
    /// Time since boot.
    pub uptime: MetricState<Duration>,
    /// Wall-clock boot time.
    pub boot_time: MetricState<SystemTime>,
    /// Heuristic container/VM classification.
    pub environment: MetricState<HostEnvironment>,
}

impl HostSnapshot {
    /// A snapshot with nothing resolved yet.
    #[must_use]
    pub const fn warming_up() -> Self {
        Self {
            hostname: MetricState::WarmingUp,
            os_name: MetricState::WarmingUp,
            os_version: MetricState::WarmingUp,
            kernel_version: MetricState::WarmingUp,
            arch: std::env::consts::ARCH,
            cpu_brand: MetricState::WarmingUp,
            uptime: MetricState::WarmingUp,
            boot_time: MetricState::WarmingUp,
            environment: MetricState::WarmingUp,
        }
    }

    /// The host name for the header, or a neutral placeholder.
    ///
    /// §5.5 puts the host name in the title bar; an empty title would look
    /// broken, and "unknown" is honest.
    #[must_use]
    pub fn display_hostname(&self) -> &str {
        self.hostname
            .displayable()
            .map_or("unknown", |(name, _)| name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_is_known_at_compile_time_and_never_unavailable() {
        let host = HostSnapshot::warming_up();
        assert!(!host.arch.is_empty());
        assert!(
            [
                "aarch64",
                "x86_64",
                "arm",
                "x86",
                "powerpc64",
                "riscv64",
                "s390x",
                "loongarch64"
            ]
            .contains(&host.arch),
            "unexpected arch {}",
            host.arch
        );
    }

    #[test]
    fn an_unresolved_hostname_renders_as_unknown_not_as_an_empty_title() {
        let host = HostSnapshot::warming_up();
        assert_eq!(host.display_hostname(), "unknown");
    }

    #[test]
    fn a_stale_hostname_is_still_displayable() {
        let mut host = HostSnapshot::warming_up();
        host.hostname = MetricState::Available("dev-mbp".into()).into_stale(Duration::from_secs(5));
        assert_eq!(host.display_hostname(), "dev-mbp");
    }

    #[test]
    fn absence_of_evidence_is_not_reported_as_bare_metal() {
        assert_eq!(EnvironmentKind::default(), EnvironmentKind::NoEvidenceFound);
        assert!(
            EnvironmentKind::NoEvidenceFound
                .label()
                .contains("evidence")
        );
    }

    #[test]
    fn an_environment_classification_carries_its_evidence_and_confidence() {
        let env = HostEnvironment {
            kind: EnvironmentKind::Container,
            evidence: "/proc/1/cgroup names docker".into(),
            confidence: Confidence::High,
        };
        assert!(!env.evidence.is_empty());
        assert_eq!(env.confidence, Confidence::High);
    }
}
