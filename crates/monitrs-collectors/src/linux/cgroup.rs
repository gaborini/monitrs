//! cgroups: membership, container identity, and the limits §9.2 requires to be
//! kept separate from host totals.
//!
//! Three separate jobs live here, and the specification treats them differently.
//!
//! **Membership** (`/proc/<pid>/cgroup`) tells us which cgroup a process is in, and
//! §7.5 wants it on the Inspect screen. Parsing it also answers "is this cgroup v2?"
//! without touching the filesystem: a `0::` entry is the unified hierarchy.
//!
//! **Container identity** is derived from that path. It is a *heuristic* — the path
//! is a naming convention of each runtime, not a kernel guarantee — so
//! [`ContainerIdentity`] records which convention matched, and the environment
//! classification carries a [`Confidence`].
//!
//! **Limits** (`memory.max`, `memory.current`, `cpu.max`) are the numbers that make
//! a container's memory panel honest. The critical rule is the `max` sentinel:
//! cgroup v2 writes the literal string `max` for "unlimited" and cgroup v1 writes
//! `9223372036854771712`. Reading either as a number would report a 8-exbibyte
//! limit — or, worse, a saturated parse would report a tiny one — and §9.2's
//! requirement that limits be shown *alongside* host totals only means something if
//! an absent limit stays absent.

use monitrs_core::model::{
    Confidence, CpuQuota, EnvironmentKind, HostEnvironment, MetricState, UnavailableReason,
};

use crate::linux::parse::{
    ParseFailure, ParseResult, fields, lines, parse_u64, to_text, trim_ascii,
};

/// The cgroup v1 "unlimited" sentinel: `PAGE_COUNTER_MAX * PAGE_SIZE` on a 64-bit
/// kernel with 4 KiB pages.
///
/// Any value at or above this is treated as unlimited. The comparison is `>=`
/// rather than `==` because the exact figure depends on the page size, so a
/// 64 KiB-page kernel writes a different — and even larger — number.
pub const CGROUP_V1_UNLIMITED: u64 = 0x7FFF_FFFF_FFFF_F000;

/// Which controller hierarchy a process's cgroup entry came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CgroupVersion {
    /// The unified hierarchy: a `0::` line is present.
    V2,
    /// Named controllers only: no `0::` line.
    V1,
    /// Both, which is what a `systemd` host in hybrid mode looks like.
    Hybrid,
}

// `ContainerRuntime` and `ContainerIdentity` live in `monitrs-core::model` so that
// `HostEnvironment` can carry an identity: a view has to render the container's name
// without depending on the Linux collector. Re-exported here because recognising one
// from a cgroup path is this module's job and `cgroup::ContainerIdentity` is where a
// reader of this file expects to find it.
pub use monitrs_core::model::{ContainerIdentity, ContainerRuntime};

/// One line of `/proc/<pid>/cgroup`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CgroupEntry {
    /// Hierarchy id. `0` for the unified hierarchy.
    pub hierarchy: u64,
    /// Comma-separated controller list, empty for the unified hierarchy.
    pub controllers: Box<str>,
    /// The cgroup path, relative to the hierarchy root.
    pub path: Box<str>,
}

/// A process's cgroup membership.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CgroupMembership {
    /// Every parsed line, in file order.
    pub entries: Vec<CgroupEntry>,
}

impl CgroupMembership {
    /// Which hierarchy version this process is in.
    ///
    /// Returns `None` for an empty membership, which is what a process in the root
    /// of a system with cgroups disabled looks like.
    #[must_use]
    pub fn version(&self) -> Option<CgroupVersion> {
        let unified = self.entries.iter().any(|entry| entry.hierarchy == 0);
        let named = self.entries.iter().any(|entry| entry.hierarchy != 0);
        match (unified, named) {
            (true, true) => Some(CgroupVersion::Hybrid),
            (true, false) => Some(CgroupVersion::V2),
            (false, true) => Some(CgroupVersion::V1),
            (false, false) => None,
        }
    }

    /// The path a monitor should show, preferring the unified hierarchy.
    ///
    /// On a hybrid host the v1 paths duplicate the v2 one for most controllers, so
    /// showing the unified path is both shorter and the one that matches the limits
    /// this module reads.
    #[must_use]
    pub fn primary_path(&self) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.hierarchy == 0)
            .or_else(|| self.entries.first())
            .map(|entry| &*entry.path)
    }

    /// The container this process appears to belong to, if any.
    ///
    /// Checks every entry, because on a hybrid host the v1 `name=systemd` line
    /// often carries the container path while the unified line does not.
    #[must_use]
    pub fn container(&self) -> Option<ContainerIdentity> {
        self.entries
            .iter()
            .find_map(|entry| identify_container(&entry.path))
    }
}

/// Parses `/proc/<pid>/cgroup`.
///
/// Each line is `hierarchy:controllers:path`. The path may itself contain colons
/// (a cgroup name is almost unrestricted), so the split takes the *first two*
/// colons and treats the remainder as the path.
///
/// Unparseable lines are skipped rather than failing the file: an unreadable
/// controller line must not cost the container identity that a later line carries.
/// An empty file yields an empty membership, which [`CgroupMembership::version`]
/// reports as `None`.
pub fn parse_pid_cgroup(bytes: &[u8]) -> ParseResult<CgroupMembership> {
    let mut membership = CgroupMembership::default();
    for line in lines(bytes) {
        let mut parts = line.splitn(3, |byte| *byte == b':');
        let Some(hierarchy) = parts.next() else {
            continue;
        };
        let Some(controllers) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        let Ok(hierarchy) = parse_u64(hierarchy, "cgroup.hierarchy") else {
            continue;
        };
        membership.entries.push(CgroupEntry {
            hierarchy,
            controllers: to_text(controllers),
            path: to_text(trim_ascii(path)),
        });
    }
    Ok(membership)
}

/// The shortest identifier accepted as a container id.
///
/// Twelve hex characters is what every runtime abbreviates to, and requiring hex
/// keeps `docker.service` — the daemon's own unit — from being read as a container.
const MIN_CONTAINER_ID: usize = 12;

/// Recognises a container from one cgroup path.
fn identify_container(path: &str) -> Option<ContainerIdentity> {
    let kubernetes = path
        .split('/')
        .any(|segment| segment.starts_with("kubepods"));

    for segment in path.split('/').rev() {
        let trimmed = segment
            .strip_suffix(".scope")
            .or_else(|| segment.strip_suffix(".slice"))
            .unwrap_or(segment);

        // `lxc.payload.<name>` and `lxc.monitor.<name>` name the container directly
        // and its id is a human-chosen name, not a digest.
        if let Some(name) = trimmed
            .strip_prefix("lxc.payload.")
            .or_else(|| trimmed.strip_prefix("lxc.monitor."))
            && !name.is_empty()
        {
            return Some(ContainerIdentity {
                runtime: ContainerRuntime::Lxc,
                id: name.into(),
                kubernetes,
            });
        }

        for (prefix, runtime) in [
            ("cri-containerd-", ContainerRuntime::Containerd),
            ("containerd-", ContainerRuntime::Containerd),
            ("docker-", ContainerRuntime::Docker),
            ("crio-", ContainerRuntime::CriO),
            ("libpod-", ContainerRuntime::Podman),
            ("machine-", ContainerRuntime::SystemdMachine),
        ] {
            if let Some(id) = trimmed.strip_prefix(prefix)
                && is_plausible_container_id(id, runtime)
            {
                return Some(ContainerIdentity {
                    runtime,
                    id: id.into(),
                    kubernetes,
                });
            }
        }
    }

    // The cgroup v1 layout `/docker/<id>` and `/lxc/<name>` put the runtime in the
    // parent segment instead of prefixing the id.
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    while let Some(segment) = segments.next() {
        let runtime = match segment {
            "docker" => ContainerRuntime::Docker,
            "lxc" => ContainerRuntime::Lxc,
            _ => continue,
        };
        if let Some(id) = segments.next()
            && is_plausible_container_id(id, runtime)
        {
            return Some(ContainerIdentity {
                runtime,
                id: id.into(),
                kubernetes,
            });
        }
    }
    None
}

/// Whether `id` looks like a real container identifier.
///
/// Digest-based runtimes must produce hex; `lxc` and `systemd-machine` names are
/// human-chosen, so they only have to be non-empty. Without the hex requirement,
/// `/system.slice/docker.service` — the Docker *daemon* — would be reported as a
/// container, which would label the whole host as containerised.
fn is_plausible_container_id(id: &str, runtime: ContainerRuntime) -> bool {
    match runtime {
        ContainerRuntime::Lxc | ContainerRuntime::SystemdMachine => !id.is_empty(),
        _ => {
            id.len() >= MIN_CONTAINER_ID
                && id.chars().all(|character| character.is_ascii_hexdigit())
        }
    }
}

/// A cgroup resource limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CgroupLimit {
    /// No limit is configured. **Not** a very large number (§9.2).
    Unlimited,
    /// A configured limit in bytes.
    Bytes(u64),
}

impl CgroupLimit {
    /// Interprets a raw byte count, folding both unlimited sentinels away.
    #[must_use]
    pub const fn from_raw(value: u64) -> Self {
        if value >= CGROUP_V1_UNLIMITED {
            Self::Unlimited
        } else {
            Self::Bytes(value)
        }
    }

    /// The limit as a metric state.
    ///
    /// [`CgroupLimit::Unlimited`] becomes [`MetricState::Unsupported`] rather than a
    /// number: there is no limit to report, and
    /// [`MemorySnapshot::effective_limit_bytes`](monitrs_core::model::MemorySnapshot::effective_limit_bytes)
    /// must fall back to the host total (§9.2).
    #[must_use]
    pub const fn state(self) -> MetricState<u64> {
        match self {
            Self::Unlimited => MetricState::Unsupported,
            Self::Bytes(bytes) => MetricState::Available(bytes),
        }
    }

    /// The configured byte limit, if there is one.
    #[must_use]
    pub const fn bytes(self) -> Option<u64> {
        match self {
            Self::Unlimited => None,
            Self::Bytes(bytes) => Some(bytes),
        }
    }
}

/// Parses a cgroup v2 `memory.max` (or v1 `memory.limit_in_bytes`).
pub fn parse_memory_max(bytes: &[u8]) -> ParseResult<CgroupLimit> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return Err(ParseFailure::Empty);
    }
    if trimmed == b"max" {
        return Ok(CgroupLimit::Unlimited);
    }
    Ok(CgroupLimit::from_raw(parse_u64(trimmed, "memory.max")?))
}

/// Parses a cgroup v2 `memory.current`.
pub fn parse_memory_current(bytes: &[u8]) -> ParseResult<u64> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return Err(ParseFailure::Empty);
    }
    parse_u64(trimmed, "memory.current")
}

/// A cgroup v2 CPU bandwidth limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuMax {
    /// Microseconds of CPU allowed per period, or `None` for `max`.
    pub quota_us: Option<u64>,
    /// The accounting period in microseconds.
    pub period_us: u64,
}

impl CpuMax {
    /// The limit expressed as a number of CPUs, e.g. `1.5`.
    ///
    /// This is the figure that belongs next to the logical CPU count on the Inspect
    /// screen: a container limited to 1.5 CPUs on a 64-CPU host is not "2% of the
    /// machine", it is a hard ceiling that a process hitting it will be throttled
    /// against. Returns `None` when unlimited or when the period is zero.
    #[must_use]
    pub fn effective_cpus(&self) -> Option<f32> {
        let quota = self.quota_us?;
        if self.period_us == 0 {
            return None;
        }
        // Narrowing to f32: this is displayed with one decimal.
        #[allow(clippy::cast_possible_truncation)]
        let cpus = (quota as f64 / self.period_us as f64) as f32;
        cpus.is_finite().then_some(cpus)
    }

    /// The quota as a metric state, for [`CpuSnapshot::cgroup_quota`].
    ///
    /// A `max` quota becomes [`MetricState::Unsupported`] — there is no ceiling to
    /// report and [`CpuSnapshot::effective_cores`] must fall back to the host's CPU
    /// count — exactly as [`CgroupLimit::state`] does for `memory.max`.
    ///
    /// A period of zero is *not* folded into "unlimited", because that reads as "no
    /// limit is configured" when a limit plainly is: the file said so, in a form this
    /// kernel should never write. It becomes [`UnavailableReason::ParseFailed`], so the
    /// user is told the limit could not be read rather than told there isn't one.
    ///
    /// [`CpuSnapshot::cgroup_quota`]: monitrs_core::model::CpuSnapshot::cgroup_quota
    /// [`CpuSnapshot::effective_cores`]: monitrs_core::model::CpuSnapshot::effective_cores
    #[must_use]
    pub fn state(&self) -> MetricState<CpuQuota> {
        let Some(quota_us) = self.quota_us else {
            return MetricState::Unsupported;
        };
        CpuQuota::new(quota_us, self.period_us).map_or(
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            MetricState::Available,
        )
    }
}

/// Parses a cgroup v2 `cpu.max`, which is `"<quota|max> <period>"`.
///
/// A single field is a failure rather than a defaulted period: the kernel always
/// writes both, so one field means this is not the file we think it is.
pub fn parse_cpu_max(bytes: &[u8]) -> ParseResult<CpuMax> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut parts = fields(trimmed);
    let quota_field = parts.next().ok_or(ParseFailure::Truncated("cpu.max"))?;
    let period_field = parts.next().ok_or(ParseFailure::Truncated("cpu.max"))?;
    let quota_us = if quota_field == b"max" {
        None
    } else {
        Some(parse_u64(quota_field, "cpu.max.quota")?)
    };
    Ok(CpuMax {
        quota_us,
        period_us: parse_u64(period_field, "cpu.max.period")?,
    })
}

/// Parses `/sys/class/dmi/id/sys_vendor` into a hypervisor name.
///
/// Returns `None` for a vendor string that names no hypervisor this function knows.
/// That is deliberately *not* evidence of bare metal: a hypervisor can pass the host
/// vendor through, and §7.5's rule that there is no bare-metal conclusion to draw is
/// exactly why [`EnvironmentKind`] has no such variant.
#[must_use]
pub fn parse_dmi_hypervisor(bytes: &[u8]) -> Option<Box<str>> {
    let vendor = to_text(trim_ascii(bytes));
    if vendor.is_empty() {
        return None;
    }
    // Deliberately conservative. `Oracle Corporation` is absent even though
    // VirtualBox is an Oracle product, because Oracle also ships physical servers
    // that report exactly that string — and `innotek GmbH` is what VirtualBox
    // actually writes. `Amazon EC2` stays, even though bare-metal EC2 instances
    // report it too, which is one more reason the confidence is only medium.
    const HYPERVISORS: [(&str, &str); 7] = [
        ("qemu", "QEMU/KVM"),
        ("kvm", "QEMU/KVM"),
        ("vmware", "VMware"),
        ("xen", "Xen"),
        ("microsoft corporation", "Hyper-V"),
        ("innotek", "VirtualBox"),
        ("amazon ec2", "Amazon EC2"),
    ];
    let lower = vendor.to_ascii_lowercase();
    HYPERVISORS
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, name)| Box::<str>::from(*name))
}

/// Classifies the host environment from cgroup and DMI evidence (§7.5).
///
/// Three rules shape this function, and all three come from the specification.
///
/// * **It is labelled a heuristic and carries its evidence.** The caller renders
///   [`HostEnvironment::evidence`] next to the conclusion so a user can judge it.
/// * **There is no bare-metal conclusion.** Absence of evidence is
///   [`EnvironmentKind::NoEvidenceFound`] with [`Confidence::Low`], never "physical
///   machine": a paravirtualised guest can look exactly like hardware from inside.
/// * **Container evidence outranks VM evidence.** A container inside a VM is the
///   common case in production, and the container is the boundary that explains the
///   limits the user is looking at. The evidence string names both so nothing is
///   hidden.
#[must_use]
pub fn classify_environment(
    membership: &CgroupMembership,
    container_marker_present: bool,
    hypervisor: Option<&str>,
) -> HostEnvironment {
    if let Some(container) = membership.container() {
        let mut evidence = format!("cgroup path names {}", container.label());
        if let Some(hypervisor) = hypervisor {
            evidence.push_str(&format!("; DMI vendor also names {hypervisor}"));
        }
        return HostEnvironment {
            kind: EnvironmentKind::Container,
            evidence: evidence.into(),
            // A runtime-specific path with a digest id is about as direct as an
            // inference gets, but it is still an inference from a naming
            // convention, so `High` rather than certainty.
            confidence: Confidence::High,
            container: Some(container),
        };
    }

    if container_marker_present {
        return HostEnvironment {
            kind: EnvironmentKind::Container,
            evidence: "container marker file present in the filesystem root".into(),
            // The marker is a convention of one runtime and can be left behind in
            // an image, so this is weaker than a live cgroup path.
            confidence: Confidence::Medium,
            // A marker file says "a container" without saying which one. Naming an
            // unknown runtime here would invent an identity out of its absence.
            container: None,
        };
    }

    if let Some(hypervisor) = hypervisor {
        return HostEnvironment {
            kind: EnvironmentKind::VirtualMachine,
            evidence: format!("DMI vendor names {hypervisor}").into(),
            // DMI strings are set by the hypervisor and are usually right, but a
            // hardware vendor can also ship a matching string.
            confidence: Confidence::Medium,
            container: None,
        };
    }

    HostEnvironment {
        kind: EnvironmentKind::NoEvidenceFound,
        evidence: match membership.primary_path() {
            Some(path) => format!("cgroup {path} names no runtime; DMI names no hypervisor").into(),
            None => "no cgroup membership and no hypervisor DMI string".into(),
        },
        confidence: Confidence::Low,
        container: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    fn membership(bytes: &[u8]) -> CgroupMembership {
        parse_pid_cgroup(bytes).expect("cgroup parsing never fails outright")
    }

    #[test]
    fn a_unified_line_is_detected_as_cgroup_v2() {
        let parsed = membership(fixtures::CGROUP_V2_DOCKER);
        assert_eq!(parsed.version(), Some(CgroupVersion::V2));
        assert_eq!(
            parsed.primary_path(),
            Some(
                "/system.slice/docker-3f4a1b2c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f2a.scope"
            )
        );
    }

    #[test]
    fn named_controllers_only_are_detected_as_cgroup_v1() {
        let parsed = membership(fixtures::CGROUP_V1_DOCKER);
        assert_eq!(parsed.version(), Some(CgroupVersion::V1));
        assert_eq!(parsed.entries.len(), 6);
    }

    #[test]
    fn a_host_running_both_hierarchies_is_reported_as_hybrid() {
        let parsed = membership(fixtures::CGROUP_HYBRID_PODMAN);
        assert_eq!(parsed.version(), Some(CgroupVersion::Hybrid));
    }

    #[test]
    fn an_empty_membership_has_no_version_rather_than_a_guessed_one() {
        assert_eq!(membership(b"").version(), None);
        assert_eq!(membership(b"").primary_path(), None);
    }

    #[test]
    fn docker_containerd_podman_and_lxc_paths_are_all_recognised() {
        let docker = membership(fixtures::CGROUP_V2_DOCKER)
            .container()
            .expect("docker path");
        assert_eq!(docker.runtime, ContainerRuntime::Docker);
        assert_eq!(docker.short_id(), "3f4a1b2c9d8e");
        assert!(!docker.kubernetes);
        assert_eq!(docker.label(), "docker 3f4a1b2c9d8e");

        let v1 = membership(fixtures::CGROUP_V1_DOCKER)
            .container()
            .expect("v1 docker path");
        assert_eq!(v1.runtime, ContainerRuntime::Docker);
        assert_eq!(v1.short_id(), "3f4a1b2c9d8e");

        let podman = membership(fixtures::CGROUP_HYBRID_PODMAN)
            .container()
            .expect("podman path");
        assert_eq!(podman.runtime, ContainerRuntime::Podman);

        let lxc = membership(fixtures::CGROUP_V2_LXC)
            .container()
            .expect("lxc path");
        assert_eq!(lxc.runtime, ContainerRuntime::Lxc);
        assert_eq!(&*lxc.id, "web01");
    }

    #[test]
    fn a_kubernetes_pod_is_recognised_together_with_its_runtime() {
        let container = membership(fixtures::CGROUP_V2_KUBERNETES)
            .container()
            .expect("kubernetes path");
        assert_eq!(container.runtime, ContainerRuntime::Containerd);
        assert!(container.kubernetes);
        assert!(container.label().starts_with("kubernetes/containerd "));
    }

    #[test]
    fn an_ordinary_login_session_is_not_a_container() {
        assert_eq!(
            membership(fixtures::CGROUP_V2_USER_SESSION).container(),
            None
        );
        assert_eq!(membership(fixtures::CGROUP_V2_ROOT).container(), None);
    }

    #[test]
    fn the_docker_daemons_own_unit_is_not_mistaken_for_a_container() {
        // `/system.slice/docker.service` is the daemon. Reading it as a container
        // would label an ordinary Docker *host* as containerised.
        assert_eq!(
            membership(b"0::/system.slice/docker.service\n").container(),
            None
        );
        assert_eq!(membership(b"0::/docker\n").container(), None);
        assert_eq!(
            membership(b"0::/docker/short\n").container(),
            None,
            "an id too short to be a digest is not evidence"
        );
        assert_eq!(
            membership(b"0::/system.slice/docker-not-hexadecimal-at-all.scope\n").container(),
            None
        );
    }

    #[test]
    fn a_path_containing_colons_keeps_its_colons() {
        let parsed = membership(b"1:name=systemd:/weird:path/here\n");
        assert_eq!(parsed.primary_path(), Some("/weird:path/here"));
        assert_eq!(
            parsed.entries.first().map(|e| &*e.controllers),
            Some("name=systemd")
        );
    }

    #[test]
    fn malformed_lines_are_skipped_rather_than_failing_the_file() {
        let parsed = membership(fixtures::CGROUP_MALFORMED);
        assert!(
            parsed.entries.len() <= 1,
            "only a well-formed line may survive, got {:?}",
            parsed.entries
        );
    }

    #[test]
    fn the_max_sentinel_never_becomes_a_number() {
        // §9.2's explicit case. Both spellings of "unlimited" must vanish rather
        // than becoming an 8 EiB — or, after a saturating parse, a tiny — limit.
        assert_eq!(
            parse_memory_max(fixtures::CGROUP_MEMORY_MAX_UNLIMITED),
            Ok(CgroupLimit::Unlimited)
        );
        assert_eq!(
            parse_memory_max(fixtures::CGROUP_MEMORY_MAX_V1_SENTINEL),
            Ok(CgroupLimit::Unlimited)
        );
        for unlimited in [
            CgroupLimit::from_raw(CGROUP_V1_UNLIMITED),
            CgroupLimit::from_raw(u64::MAX),
        ] {
            assert_eq!(unlimited, CgroupLimit::Unlimited);
            assert_eq!(unlimited.bytes(), None);
            assert!(unlimited.state().is_unsupported());
            assert!(unlimited.state().fresh().is_none());
        }
    }

    #[test]
    fn a_real_limit_is_reported_as_a_number() {
        let limit = parse_memory_max(fixtures::CGROUP_MEMORY_MAX_LIMITED).expect("valid");
        assert_eq!(limit, CgroupLimit::Bytes(2 * 1024 * 1024 * 1024));
        assert_eq!(limit.state().fresh(), Some(&(2 * 1024 * 1024 * 1024)));
    }

    #[test]
    fn an_unlimited_cgroup_leaves_the_host_total_as_the_effective_ceiling() {
        use monitrs_core::model::{MemorySemantics, MemorySnapshot};
        let host_total = 32 * 1024 * 1024 * 1024;
        let mut memory = MemorySnapshot::warming_up(host_total, MemorySemantics::LinuxMemAvailable);
        memory.cgroup_limit_bytes = parse_memory_max(fixtures::CGROUP_MEMORY_MAX_UNLIMITED)
            .expect("valid")
            .state();
        assert_eq!(memory.effective_limit_bytes(), host_total);

        memory.cgroup_limit_bytes = parse_memory_max(fixtures::CGROUP_MEMORY_MAX_LIMITED)
            .expect("valid")
            .state();
        assert_eq!(memory.effective_limit_bytes(), 2 * 1024 * 1024 * 1024);
        assert_eq!(
            memory.total_bytes, host_total,
            "§9.2: the host total stays observable alongside the limit"
        );
    }

    #[test]
    fn memory_current_is_a_plain_counter() {
        assert_eq!(
            parse_memory_current(fixtures::CGROUP_MEMORY_CURRENT),
            Ok(1_503_238_553)
        );
        assert_eq!(parse_memory_current(b""), Err(ParseFailure::Empty));
        assert!(parse_memory_current(b"max\n").is_err());
    }

    #[test]
    fn cpu_max_yields_a_cpu_count_and_max_yields_none() {
        let limited = parse_cpu_max(fixtures::CGROUP_CPU_MAX_LIMITED).expect("valid");
        assert_eq!(limited.quota_us, Some(150_000));
        assert_eq!(limited.period_us, 100_000);
        let cpus = limited.effective_cpus().expect("a quota is configured");
        assert!((cpus - 1.5).abs() < f32::EPSILON);

        let unlimited = parse_cpu_max(fixtures::CGROUP_CPU_MAX_UNLIMITED).expect("valid");
        assert_eq!(unlimited.quota_us, None);
        assert_eq!(unlimited.effective_cpus(), None);
    }

    #[test]
    fn a_one_field_cpu_max_is_a_failure_rather_than_a_defaulted_period() {
        assert_eq!(
            parse_cpu_max(fixtures::CGROUP_CPU_MAX_MALFORMED),
            Err(ParseFailure::Truncated("cpu.max"))
        );
        assert_eq!(parse_cpu_max(b""), Err(ParseFailure::Empty));
        assert_eq!(
            CpuMax {
                quota_us: Some(1),
                period_us: 0
            }
            .effective_cpus(),
            None
        );
    }

    #[test]
    fn a_container_conclusion_is_high_confidence_and_names_its_evidence() {
        let environment =
            classify_environment(&membership(fixtures::CGROUP_V2_DOCKER), false, None);
        assert_eq!(environment.kind, EnvironmentKind::Container);
        assert_eq!(environment.confidence, Confidence::High);
        assert!(environment.evidence.contains("docker"));
        assert!(!environment.evidence.is_empty());
    }

    #[test]
    fn a_container_inside_a_vm_reports_the_container_but_names_both() {
        let environment = classify_environment(
            &membership(fixtures::CGROUP_V2_DOCKER),
            true,
            Some("QEMU/KVM"),
        );
        assert_eq!(environment.kind, EnvironmentKind::Container);
        assert!(environment.evidence.contains("docker"));
        assert!(
            environment.evidence.contains("QEMU/KVM"),
            "the VM evidence must not be hidden: {}",
            environment.evidence
        );
    }

    #[test]
    fn a_marker_file_alone_is_a_weaker_container_conclusion() {
        let environment = classify_environment(&membership(fixtures::CGROUP_V2_ROOT), true, None);
        assert_eq!(environment.kind, EnvironmentKind::Container);
        assert_eq!(environment.confidence, Confidence::Medium);
    }

    #[test]
    fn a_hypervisor_vendor_alone_is_a_virtual_machine() {
        let environment = classify_environment(
            &membership(fixtures::CGROUP_V2_USER_SESSION),
            false,
            parse_dmi_hypervisor(fixtures::DMI_SYS_VENDOR_QEMU).as_deref(),
        );
        assert_eq!(environment.kind, EnvironmentKind::VirtualMachine);
        assert_eq!(environment.confidence, Confidence::Medium);
        assert!(environment.evidence.contains("QEMU/KVM"));
    }

    #[test]
    fn no_evidence_is_never_reported_as_bare_metal() {
        // §7.5: there is no bare-metal conclusion to draw, and the type has no
        // variant for one.
        let environment = classify_environment(
            &membership(fixtures::CGROUP_V2_USER_SESSION),
            false,
            parse_dmi_hypervisor(fixtures::DMI_SYS_VENDOR_PHYSICAL).as_deref(),
        );
        assert_eq!(environment.kind, EnvironmentKind::NoEvidenceFound);
        assert_eq!(environment.confidence, Confidence::Low);
        assert!(environment.evidence.contains("no hypervisor"));
        assert!(!environment.evidence.to_lowercase().contains("bare metal"));
        assert!(!environment.evidence.to_lowercase().contains("physical"));
    }

    #[test]
    fn known_hypervisor_vendors_are_recognised_and_others_are_not() {
        assert_eq!(
            parse_dmi_hypervisor(fixtures::DMI_SYS_VENDOR_QEMU).as_deref(),
            Some("QEMU/KVM")
        );
        assert_eq!(
            parse_dmi_hypervisor(b"VMware, Inc.\n").as_deref(),
            Some("VMware")
        );
        assert_eq!(parse_dmi_hypervisor(b"Xen\n").as_deref(), Some("Xen"));
        assert_eq!(
            parse_dmi_hypervisor(b"Microsoft Corporation\n").as_deref(),
            Some("Hyper-V")
        );
        assert_eq!(
            parse_dmi_hypervisor(fixtures::DMI_SYS_VENDOR_PHYSICAL),
            None
        );
        assert_eq!(parse_dmi_hypervisor(b"\n"), None);
    }

    #[test]
    fn a_short_id_never_splits_a_character_or_panics_on_a_short_one() {
        let short = ContainerIdentity {
            runtime: ContainerRuntime::Docker,
            id: "abc".into(),
            kubernetes: false,
        };
        assert_eq!(short.short_id(), "abc");
        let empty = ContainerIdentity {
            runtime: ContainerRuntime::Unknown,
            id: "".into(),
            kubernetes: false,
        };
        assert_eq!(empty.short_id(), "");
    }
}
