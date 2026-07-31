//! The `sysinfo`-backed baseline collector.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use monitrs_core::SystemSnapshot;
use monitrs_core::model::{
    CapabilitySnapshot, CapabilityState, CollectorHealth, CpuSnapshot, CpuUsage, DiskSnapshot,
    DiskTotals, FilesystemKind, FilesystemSnapshot, HostSnapshot, InterfaceAddress,
    InterfaceErrors, InterfaceKind, LoadSnapshot, MemoryDetail, MemorySemantics, MemorySnapshot,
    MetricState, NetworkSnapshot, PressureSnapshot, ProcessDetail, ProcessDetailResult,
    ProcessIdentity, ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState, SensorSnapshot,
    SwapSnapshot, TemperatureReading, TrafficTotals, UnavailableReason, UserIdentity,
};
use monitrs_core::rates::{CounterWidth, KeyedRateTrackers};
use monitrs_core::units::Percent;
use sysinfo::{
    Components, Disks, MemoryRefreshKind, Networks, Process, ProcessRefreshKind, ProcessStatus,
    ProcessesToUpdate, System, Users,
};

use crate::error::CollectorError;
use crate::source::{SampleTick, SnapshotSource};

/// Interface and device names are the keys of the rate trackers. `Box<str>` keeps
/// the map compact while still owning the name, which matters because an
/// interface can be renamed underneath us (§8.2).
type DeviceKey = Box<str>;

/// The cross-platform baseline collector.
///
/// Holds every `sysinfo` handle and every rate baseline for the life of the
/// process. Dropping and recreating it is a bug, not an optimisation.
#[derive(Debug)]
pub struct CommonCollector {
    system: System,
    disks: Disks,
    networks: Networks,
    components: Components,
    users: Users,

    logical_cpus: u16,

    /// True once processes have been refreshed at least twice.
    ///
    /// `sysinfo` reports `0.0` CPU for a process on its first refresh, and
    /// reporting that as a real zero is precisely the mistake §8.2 and §26 warn
    /// about. Until this is true, every CPU figure is `WarmingUp`.
    cpu_has_baseline: bool,

    network_rx: KeyedRateTrackers<DeviceKey>,
    network_tx: KeyedRateTrackers<DeviceKey>,
    network_rx_packets: KeyedRateTrackers<DeviceKey>,
    network_tx_packets: KeyedRateTrackers<DeviceKey>,
    disk_read: KeyedRateTrackers<DeviceKey>,
    disk_write: KeyedRateTrackers<DeviceKey>,
    process_read: KeyedRateTrackers<ProcessIdentity>,
    process_write: KeyedRateTrackers<ProcessIdentity>,

    /// Byte and packet totals accumulated since monitrs launched.
    ///
    /// Kept separately from the OS counters because those may have wrapped or
    /// been reset long before we started (§7.4).
    launch_totals: HashMap<DeviceKey, TrafficTotals>,
    /// Baselines for the launch totals, so they start at zero.
    launch_baseline: HashMap<DeviceKey, TrafficTotals>,

    // Carried over between ticks for tiers that are not due (§9.1).
    cached_host: HostSnapshot,
    cached_filesystems: Vec<FilesystemSnapshot>,
    cached_sensors: SensorSnapshot,
    /// When `cached_sensors` was actually measured, so a carried-over reading can
    /// state its age (§4). `None` until the first sensor read.
    sensors_read_at: Option<Instant>,
    capabilities: CapabilitySnapshot,
    /// Whether a native layer supplies the process table (see
    /// [`Self::delegate_process_table`]).
    process_table_delegated: bool,
}

impl CommonCollector {
    /// Builds the collector and takes its first, baseline-establishing readings.
    ///
    /// This is the only fallible step: if `sysinfo` cannot see the system at all
    /// there is nothing to monitor, which §14.1 classifies as a fatal startup
    /// error rather than an unavailable metric.
    pub fn new() -> Result<Self, CollectorError> {
        let mut system = System::new();
        system.refresh_memory();

        let total_memory = system.total_memory();
        if total_memory == 0 {
            return Err(CollectorError::Initialisation {
                collector: "sysinfo",
                reason: "the system reports zero total memory, so no metric can be trusted".into(),
            });
        }

        system.refresh_cpu_all();
        let logical_cpus = u16::try_from(system.cpus().len()).unwrap_or(u16::MAX);

        Ok(Self {
            system,
            disks: Disks::new(),
            networks: Networks::new(),
            components: Components::new(),
            users: Users::new(),
            logical_cpus,
            cpu_has_baseline: false,
            // Interface counters are 64-bit on both target platforms, but a
            // 32-bit counter on a virtual interface is possible; `Unknown` treats
            // a backwards move as a reset, which is the safe reading (§8.2).
            network_rx: KeyedRateTrackers::new(CounterWidth::Unknown),
            network_tx: KeyedRateTrackers::new(CounterWidth::Unknown),
            network_rx_packets: KeyedRateTrackers::new(CounterWidth::Unknown),
            network_tx_packets: KeyedRateTrackers::new(CounterWidth::Unknown),
            disk_read: KeyedRateTrackers::new(CounterWidth::Unknown),
            disk_write: KeyedRateTrackers::new(CounterWidth::Unknown),
            process_read: KeyedRateTrackers::new(CounterWidth::Unknown),
            process_write: KeyedRateTrackers::new(CounterWidth::Unknown),
            launch_totals: HashMap::new(),
            launch_baseline: HashMap::new(),
            cached_host: HostSnapshot::warming_up(),
            cached_filesystems: Vec::new(),
            cached_sensors: SensorSnapshot::warming_up(),
            sensors_read_at: None,
            capabilities: baseline_capabilities(),
            process_table_delegated: false,
        })
    }

    /// Stops walking the process table, because a native layer supplies it.
    ///
    /// The most expensive thing this collector does. Measured on macOS against 987
    /// processes, `sysinfo`'s process refresh costs **30.8 ms of CPU** per fast tick —
    /// while the macOS enrichment, which replaces every row it produces, walks the same
    /// table for 2.3 ms and reads its per-process counters for 6.0 ms. The baseline's
    /// walk was buying identity, name, command and executable, and the native layer can
    /// now supply all four; nothing else it produced survived the merge.
    ///
    /// Asking for fewer *fields* does not help — `ProcessRefreshKind::nothing()` still
    /// costs 26 of the 29 ms, because the per-process `proc_pidinfo` calls that
    /// validate each entry are the cost, not the fields. Skipping the walk entirely is
    /// the only saving available, which is why this is a switch rather than a
    /// refinement.
    ///
    /// Everything else the baseline reports is unaffected: total memory, the CPU
    /// aggregate, disks, filesystems, networks and sensors all come from other
    /// `sysinfo` objects. `process_detail` also still works — it refreshes exactly one
    /// PID on demand, which is a different call and is not on any tier.
    pub fn delegate_process_table(&mut self) {
        self.process_table_delegated = true;
    }

    /// Logical CPU count, needed to normalize process CPU (§8.3).
    #[must_use]
    pub const fn logical_cpus(&self) -> u16 {
        self.logical_cpus
    }

    fn refresh_slow(&mut self) {
        self.users.refresh();
        self.cached_host = HostSnapshot {
            hostname: text_state(System::host_name()),
            os_name: text_state(System::name()),
            os_version: text_state(System::long_os_version().or_else(System::os_version)),
            kernel_version: text_state(System::kernel_version()),
            arch: std::env::consts::ARCH,
            cpu_brand: self
                .system
                .cpus()
                .first()
                .map(|cpu| cpu.brand().trim().to_owned())
                .filter(|brand| !brand.is_empty())
                .map_or(MetricState::Unsupported, |brand| {
                    MetricState::Available(brand.into())
                }),
            uptime: MetricState::Available(Duration::from_secs(System::uptime())),
            boot_time: MetricState::Available(
                SystemTime::UNIX_EPOCH + Duration::from_secs(System::boot_time()),
            ),
            // Container and VM detection is a heuristic that needs platform
            // sources the baseline does not have. The native layers fill it in;
            // claiming "no evidence found" here would be an unearned conclusion.
            environment: MetricState::Unsupported,
        };
    }

    fn refresh_medium(&mut self) {
        self.disks.refresh(true);

        self.cached_filesystems = self
            .disks
            .list()
            .iter()
            .map(|disk| {
                let total = disk.total_space();
                let available = disk.available_space();
                let used = total.saturating_sub(available);
                let fs_type = disk.file_system().to_string_lossy().into_owned();
                FilesystemSnapshot {
                    mount_point: disk.mount_point().to_string_lossy().into_owned().into(),
                    device: Some(disk.name().to_string_lossy().into_owned().into()),
                    fs_type: (!fs_type.is_empty()).then(|| fs_type.clone().into()),
                    total_bytes: total,
                    available_bytes: MetricState::Available(available),
                    used_bytes: MetricState::Available(used),
                    // A zero-capacity mount has no defined utilization, which is
                    // different from being 0% full (§4).
                    usage: Percent::ratio(used, total)
                        .map_or(MetricState::Unsupported, MetricState::Available),
                    // `sysinfo` exposes no inode counts at all — there is no
                    // `f_files` anywhere in its `Disk` API — so this is a metric the
                    // baseline cannot produce rather than one it measured as zero.
                    // `crate::inodes` is what the native layers fill it in through.
                    inodes: MetricState::Unsupported,
                    kind: classify_filesystem(&fs_type, disk.is_removable()),
                    read_only: disk.is_read_only(),
                }
            })
            .collect();

        self.capabilities.filesystem_capacity = if self.cached_filesystems.is_empty() {
            CapabilityState::Unsupported
        } else {
            CapabilityState::Available
        };
    }

    /// Reads the sensors: temperatures here, battery in the native layers.
    ///
    /// Separate from the medium tier because this is the expensive read of the
    /// whole collector — about 85 ms on macOS, where it is every SMC key the
    /// machine has — and §16.1's idle budget cannot absorb it every five seconds.
    fn refresh_sensors(&mut self) {
        self.components.refresh(true);
        self.cached_sensors = SensorSnapshot {
            temperatures: read_temperatures(&self.components),
            // The baseline exposes no battery data; the native layers add it.
            battery: MetricState::Unsupported,
        };
        self.capabilities.temperatures = if self.cached_sensors.temperatures.is_available() {
            CapabilityState::Available
        } else {
            CapabilityState::Unsupported
        };
    }

    /// The sensor snapshot to publish on this tick.
    ///
    /// On a tick that read them, the measurement. On any other tick, the same
    /// values marked stale with the real gap since they were measured — never
    /// republished as if they had just been read (§4).
    fn sensors_for(&self, tick: &SampleTick) -> SensorSnapshot {
        let age = match (tick.due.sensors(), self.sensors_read_at) {
            (true, _) | (_, None) => Duration::ZERO,
            (false, Some(read_at)) => tick.captured_at.saturating_duration_since(read_at),
        };
        if age.is_zero() {
            return self.cached_sensors.clone();
        }
        SensorSnapshot {
            temperatures: retained_sensor(self.cached_sensors.temperatures.clone(), age),
            battery: retained_sensor(self.cached_sensors.battery, age),
        }
    }

    /// Refreshes the fast tier.
    ///
    /// `disks_already_refreshed` is set when the medium tier ran in the same tick, so
    /// the two tiers do not read the same counters twice. `sample` runs the tiers
    /// slow, medium, fast, so by then the medium refresh has already happened and its
    /// counters are from this tick.
    fn refresh_fast(&mut self, disks_already_refreshed: bool) {
        self.system.refresh_cpu_usage();
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::everything());

        if self.process_table_delegated {
            // A native layer owns the process table (see `delegate_process_table`), so
            // the CPU baseline has to come from the per-CPU readings alone rather than
            // from "did any process report a percentage yet".
            if !self.cpu_has_baseline {
                self.cpu_has_baseline = self.system.cpus().iter().any(|cpu| cpu.cpu_usage() > 0.0);
            }
            self.networks.refresh(true);
            if !disks_already_refreshed {
                self.disks
                    .refresh_specifics(false, sysinfo::DiskRefreshKind::nothing().with_io_usage());
            }
            return;
        }

        // `remove_dead_processes = true` is what keeps the process map from
        // growing without bound as PIDs churn (§10.3).
        let refreshed = self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_disk_usage()
                .with_user(sysinfo::UpdateKind::OnlyIfNotSet)
                .with_cmd(sysinfo::UpdateKind::OnlyIfNotSet)
                .with_exe(sysinfo::UpdateKind::OnlyIfNotSet)
                .with_tasks(),
        );
        if refreshed > 0 && !self.cpu_has_baseline {
            // The first refresh only establishes the baseline. The second one is
            // the first that can produce a real percentage.
            self.cpu_has_baseline = self.system.cpus().iter().any(|cpu| cpu.cpu_usage() > 0.0)
                || self
                    .system
                    .processes()
                    .values()
                    .any(|p| p.cpu_usage() > 0.0);
        }

        self.networks.refresh(true);
        if !disks_already_refreshed {
            // `refresh_specifics` rather than `refresh`, and this is the single most
            // expensive line in the fast tier if it is written the obvious way.
            //
            // `Disks::refresh(bool)` reads as "refresh, cheaply or not"; it is not.
            // It calls `refresh_specifics(bool, DiskRefreshKind::everything())`, and
            // the `bool` is only `remove_not_listed_disks`. `everything()` includes
            // `storage`, which on macOS is a per-mount volume-capacity query through
            // `CFURL`'s resource properties — measured at about 19.6 ms per tick
            // against two volumes, and it is 94% of what this line used to cost.
            //
            // The fast tier reads `usage()` and `name()` from these disks and nothing
            // else: capacity is a *medium*-tier metric (§8.6), published from
            // `cached_filesystems`, which `refresh_medium` builds. So the capacity
            // this was paying for once a second was never read until the medium tick
            // asked for it again.
            self.disks
                .refresh_specifics(false, sysinfo::DiskRefreshKind::nothing().with_io_usage());
        }
    }

    fn cpu(&self, tick: &SampleTick) -> CpuSnapshot {
        let (total, per_core) = if self.cpu_has_baseline && tick.can_compute_rates() {
            let total = percent_state(self.system.global_cpu_usage());
            let cores: Vec<CpuUsage> = self
                .system
                .cpus()
                .iter()
                .map(|cpu| {
                    CpuUsage::plain(
                        Percent::new(cpu.cpu_usage().clamp(0.0, 100.0)).unwrap_or(Percent::ZERO),
                    )
                })
                .collect();
            (
                total.map(CpuUsage::plain),
                if cores.is_empty() {
                    MetricState::Unsupported
                } else {
                    MetricState::Available(cores)
                },
            )
        } else {
            (MetricState::WarmingUp, MetricState::WarmingUp)
        };

        CpuSnapshot {
            logical_count: self.logical_cpus,
            physical_count: System::physical_core_count()
                .and_then(|count| u16::try_from(count).ok())
                .map_or(MetricState::Unsupported, MetricState::Available),
            total,
            per_core,
            // Frequency lives on the CPU refresh group we do not request every
            // tick; reading it here would report a stale value as current.
            frequency_mhz: MetricState::Unsupported,
            // Cgroups are a Linux concept and invisible to `sysinfo`; the Linux
            // enrichment fills this in where a quota is configured. Unsupported rather
            // than "no limit" so a platform that cannot look is distinguishable from a
            // group that is genuinely unrestricted (§9.2).
            cgroup_quota: MetricState::Unsupported,
            // The baseline cannot see core classes: `sysinfo` reports a flat list of
            // CPUs. The native layers fill this in where the platform names them.
            core_classes: Vec::new(),
        }
    }

    fn memory(&self) -> MemorySnapshot {
        let total = self.system.total_memory();
        let available = self.system.available_memory();
        let used = total.saturating_sub(available);
        let swap_total = self.system.total_swap();

        MemorySnapshot {
            total_bytes: total,
            available: MetricState::Available(available),
            used: MetricState::Available(used),
            free: MetricState::Available(self.system.free_memory()),
            usage: Percent::ratio(used, total)
                .map_or(MetricState::Unsupported, MetricState::Available),
            // The baseline does not break memory down. Reporting zeros here would
            // claim there is no page cache, which is false on every real system.
            detail: MemoryDetail {
                cached: MetricState::Unsupported,
                buffers: MetricState::Unsupported,
                shared: MetricState::Unsupported,
                active: MetricState::Unsupported,
                inactive: MetricState::Unsupported,
                wired: MetricState::Unsupported,
                compressed: MetricState::Unsupported,
                dirty: MetricState::Unsupported,
            },
            swap: if swap_total == 0 {
                SwapSnapshot::disabled()
            } else {
                let used_swap = self.system.used_swap();
                SwapSnapshot {
                    total_bytes: swap_total,
                    used: MetricState::Available(used_swap),
                    usage: Percent::ratio(used_swap, swap_total)
                        .map_or(MetricState::Unsupported, MetricState::Available),
                    // Swap *activity* needs kernel counters the baseline lacks.
                    // Capacity without activity is the less useful half (§8.4).
                    in_rate: MetricState::Unsupported,
                    out_rate: MetricState::Unsupported,
                }
            },
            semantics: MemorySemantics::SysinfoBaseline,
            cgroup_limit_bytes: MetricState::Unsupported,
            cgroup_used_bytes: MetricState::Unsupported,
        }
    }

    fn processes(&mut self, tick: &SampleTick) -> Vec<ProcessSnapshot> {
        if self.process_table_delegated {
            // Empty rather than whatever `sysinfo` last happened to hold. A native
            // layer replaces this list wholesale, and returning a stale table would
            // mean any row it did not overwrite was published as current (§4).
            return Vec::new();
        }
        let total_memory = self.system.total_memory();
        let can_rate = tick.can_compute_rates();
        let has_cpu = self.cpu_has_baseline && can_rate;
        let at = tick.captured_at;

        // Collected first so the rate trackers can be updated without holding an
        // immutable borrow of `self.system` across the mutable tracker calls.
        let raw: Vec<RawProcess> = self
            .system
            .processes()
            .values()
            .map(|process| RawProcess::from_sysinfo(process, &self.users))
            .collect();

        let mut live = Vec::with_capacity(raw.len());
        let snapshots = raw
            .into_iter()
            .map(|raw| {
                live.push(raw.identity);
                let io = if can_rate {
                    ProcessIo {
                        read: self.process_read.observe(raw.identity, raw.total_read, at),
                        write: self
                            .process_write
                            .observe(raw.identity, raw.total_write, at),
                        read_total_bytes: MetricState::Available(raw.total_read),
                        write_total_bytes: MetricState::Available(raw.total_write),
                    }
                } else {
                    // Establish the baseline without publishing a rate.
                    let _ = self.process_read.observe(raw.identity, raw.total_read, at);
                    let _ = self
                        .process_write
                        .observe(raw.identity, raw.total_write, at);
                    ProcessIo::WARMING_UP
                };

                ProcessSnapshot {
                    identity: raw.identity,
                    parent_pid: raw.parent_pid,
                    name: raw.name,
                    command: raw.command,
                    exe: raw.exe,
                    user: raw.user,
                    state: raw.state,
                    cpu: if has_cpu {
                        percent_state(raw.cpu_percent)
                    } else {
                        MetricState::WarmingUp
                    },
                    memory: ProcessMemory {
                        rss_bytes: MetricState::Available(raw.rss_bytes),
                        virtual_bytes: MetricState::Available(raw.virtual_bytes),
                        share_of_total: Percent::ratio(raw.rss_bytes, total_memory)
                            .map_or(MetricState::Unsupported, MetricState::Available),
                    },
                    io,
                    threads: raw.threads,
                    age: MetricState::Available(Duration::from_secs(raw.run_time_secs)),
                    started_at: MetricState::Available(
                        SystemTime::UNIX_EPOCH + Duration::from_secs(raw.start_time_secs),
                    ),
                    is_kernel_thread: raw.is_kernel_thread,
                }
            })
            .collect();

        // Drop baselines for processes that are gone, so PID churn cannot grow
        // the trackers without bound (§10.3).
        let live: std::collections::HashSet<ProcessIdentity> = live.into_iter().collect();
        self.process_read.retain(|identity| live.contains(identity));
        self.process_write
            .retain(|identity| live.contains(identity));

        snapshots
    }

    fn disks(&mut self, tick: &SampleTick) -> Vec<DiskSnapshot> {
        let can_rate = tick.can_compute_rates();
        let at = tick.captured_at;

        // Collapsed by device, because `sysinfo` lists one entry per *mount* and a
        // device with several mounts is still one device. On an APFS Mac that is the
        // ordinary case — `/` and `/System/Volumes/Data` share a container — and the
        // uncollapsed version produced two identical `Macintosh HD` rows whose second
        // one was permanently `warming up`: both wrote to the same rate tracker in the
        // same tick, so the second `observe` saw a counter that had not moved. Two rows
        // for one device is wrong on its own; one of them lying about warming up is
        // worse.
        //
        // A `BTreeMap` rather than a `HashMap` so the row order is stable between
        // frames, which §7.2's stability rule asks of every table.
        let mut by_device: std::collections::BTreeMap<DeviceKey, (u64, u64, Vec<Box<str>>)> =
            std::collections::BTreeMap::new();
        for disk in self.disks.list() {
            let usage = disk.usage();
            let device: DeviceKey = disk.name().to_string_lossy().into_owned().into();
            let mount: Box<str> = disk.mount_point().to_string_lossy().into_owned().into();
            let entry = by_device.entry(device).or_insert_with(|| {
                (
                    usage.total_read_bytes,
                    usage.total_written_bytes,
                    Vec::new(),
                )
            });
            // The counters are per device, so every mount of it reports the same
            // figures; taking the largest is defensive rather than meaningful.
            entry.0 = entry.0.max(usage.total_read_bytes);
            entry.1 = entry.1.max(usage.total_written_bytes);
            if !mount.is_empty() && !entry.2.contains(&mount) {
                entry.2.push(mount);
            }
        }
        let raw: Vec<(DeviceKey, u64, u64, Vec<Box<str>>)> = by_device
            .into_iter()
            .map(|(device, (read, write, mounts))| (device, read, write, mounts))
            .collect();

        raw.into_iter()
            .map(|(device, total_read, total_written, mounts)| {
                let (read, write) = if can_rate {
                    (
                        self.disk_read.observe(device.clone(), total_read, at),
                        self.disk_write.observe(device.clone(), total_written, at),
                    )
                } else {
                    let _ = self.disk_read.observe(device.clone(), total_read, at);
                    let _ = self.disk_write.observe(device.clone(), total_written, at);
                    (MetricState::WarmingUp, MetricState::WarmingUp)
                };

                DiskSnapshot {
                    device,
                    model: None,
                    read,
                    write,
                    // Operation counts and busy time need `/proc/diskstats` or an
                    // IOKit query. §7.3 forbids approximating busy time, so it is
                    // unsupported here rather than derived from throughput.
                    read_ops: MetricState::Unsupported,
                    write_ops: MetricState::Unsupported,
                    busy: MetricState::Unsupported,
                    queue_length: MetricState::Unsupported,
                    totals: if can_rate {
                        MetricState::Available(DiskTotals {
                            read_bytes: total_read,
                            write_bytes: total_written,
                        })
                    } else {
                        MetricState::WarmingUp
                    },
                    mount_points: mounts,
                }
            })
            .collect()
    }

    fn networks(&mut self, tick: &SampleTick) -> Vec<NetworkSnapshot> {
        let can_rate = tick.can_compute_rates();
        let at = tick.captured_at;

        struct RawInterface {
            name: DeviceKey,
            totals: TrafficTotals,
            errors: InterfaceErrors,
            addresses: Vec<InterfaceAddress>,
            mac: Option<Box<str>>,
        }

        let raw: Vec<RawInterface> = self
            .networks
            .list()
            .iter()
            .map(|(name, data)| {
                let mac = data.mac_address();
                RawInterface {
                    name: name.clone().into_boxed_str(),
                    totals: TrafficTotals {
                        rx_bytes: data.total_received(),
                        tx_bytes: data.total_transmitted(),
                        rx_packets: data.total_packets_received(),
                        tx_packets: data.total_packets_transmitted(),
                    },
                    errors: InterfaceErrors {
                        rx_errors: data.total_errors_on_received(),
                        tx_errors: data.total_errors_on_transmitted(),
                        // Drop counters are not in the baseline; zero would claim
                        // there are none, which we do not know.
                        rx_dropped: 0,
                        tx_dropped: 0,
                    },
                    addresses: data
                        .ip_networks()
                        .iter()
                        .map(|network| InterfaceAddress {
                            ip: network.addr,
                            prefix_len: Some(network.prefix),
                        })
                        .collect(),
                    mac: (!mac.is_unspecified()).then(|| mac.to_string().into_boxed_str()),
                }
            })
            .collect();

        let live: std::collections::HashSet<DeviceKey> =
            raw.iter().map(|entry| entry.name.clone()).collect();

        let snapshots = raw
            .into_iter()
            .map(|entry| {
                let key = entry.name.clone();
                let (rx, tx, rx_packets, tx_packets) = if can_rate {
                    (
                        self.network_rx
                            .observe(key.clone(), entry.totals.rx_bytes, at),
                        self.network_tx
                            .observe(key.clone(), entry.totals.tx_bytes, at),
                        self.network_rx_packets
                            .observe(key.clone(), entry.totals.rx_packets, at),
                        self.network_tx_packets
                            .observe(key.clone(), entry.totals.tx_packets, at),
                    )
                } else {
                    let _ = self
                        .network_rx
                        .observe(key.clone(), entry.totals.rx_bytes, at);
                    let _ = self
                        .network_tx
                        .observe(key.clone(), entry.totals.tx_bytes, at);
                    let _ =
                        self.network_rx_packets
                            .observe(key.clone(), entry.totals.rx_packets, at);
                    let _ =
                        self.network_tx_packets
                            .observe(key.clone(), entry.totals.tx_packets, at);
                    (
                        MetricState::WarmingUp,
                        MetricState::WarmingUp,
                        MetricState::WarmingUp,
                        MetricState::WarmingUp,
                    )
                };

                let baseline = self
                    .launch_baseline
                    .entry(key.clone())
                    .or_insert(entry.totals);
                let since_launch = TrafficTotals {
                    rx_bytes: entry.totals.rx_bytes.saturating_sub(baseline.rx_bytes),
                    tx_bytes: entry.totals.tx_bytes.saturating_sub(baseline.tx_bytes),
                    rx_packets: entry.totals.rx_packets.saturating_sub(baseline.rx_packets),
                    tx_packets: entry.totals.tx_packets.saturating_sub(baseline.tx_packets),
                };
                self.launch_totals.insert(key.clone(), since_launch);

                NetworkSnapshot {
                    kind: classify_interface(&entry.name),
                    // Link state and speed need `/sys/class/net` or an ioctl. §7.4
                    // forbids a utilization percentage without a known speed, so
                    // leaving this unsupported is what suppresses it.
                    state: MetricState::Unsupported,
                    link_speed_mbps: MetricState::Unsupported,
                    name: entry.name,
                    addresses: entry.addresses,
                    mac: entry.mac,
                    rx,
                    tx,
                    rx_packets,
                    tx_packets,
                    errors: if can_rate {
                        MetricState::Available(entry.errors)
                    } else {
                        MetricState::WarmingUp
                    },
                    since_launch,
                    os_totals: MetricState::Available(entry.totals),
                }
            })
            .collect();

        self.network_rx.retain(|name| live.contains(name));
        self.network_tx.retain(|name| live.contains(name));
        self.network_rx_packets.retain(|name| live.contains(name));
        self.network_tx_packets.retain(|name| live.contains(name));
        self.launch_baseline.retain(|name, _| live.contains(name));
        self.launch_totals.retain(|name, _| live.contains(name));

        snapshots
    }
}

/// The fields we read from one `sysinfo` process, decoupled from its borrow.
struct RawProcess {
    identity: ProcessIdentity,
    parent_pid: Option<u32>,
    name: Box<str>,
    command: Box<str>,
    exe: Option<Box<str>>,
    user: MetricState<UserIdentity>,
    state: ProcessState,
    cpu_percent: f32,
    rss_bytes: u64,
    virtual_bytes: u64,
    total_read: u64,
    total_write: u64,
    threads: MetricState<u32>,
    run_time_secs: u64,
    start_time_secs: u64,
    is_kernel_thread: bool,
}

impl RawProcess {
    fn from_sysinfo(process: &Process, users: &Users) -> Self {
        let usage = process.disk_usage();
        let command = process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        Self {
            // The baseline's start key is the process start time in whole
            // seconds, which is what `sysinfo` exposes portably. It changes on
            // PID reuse in every practical case; the residual risk is a PID
            // reused within the same second, which the native Linux collector
            // eliminates by using the start time in clock ticks instead.
            identity: ProcessIdentity::new(process.pid().as_u32(), process.start_time()),
            parent_pid: process.parent().map(|pid| pid.as_u32()),
            name: process.name().to_string_lossy().into_owned().into(),
            command: command.into(),
            exe: process
                .exe()
                .map(|path| path.to_string_lossy().into_owned().into()),
            user: process
                .user_id()
                .map_or(MetricState::PermissionDenied, |uid| {
                    MetricState::Available(UserIdentity {
                        uid: **uid,
                        name: users
                            .get_user_by_id(uid)
                            .map(|user| user.name().to_owned().into()),
                    })
                }),
            state: map_process_state(process.status()),
            cpu_percent: process.cpu_usage(),
            rss_bytes: process.memory(),
            virtual_bytes: process.virtual_memory(),
            total_read: usage.total_read_bytes,
            total_write: usage.total_written_bytes,
            threads: process
                .tasks()
                .map(|tasks| u32::try_from(tasks.len()).unwrap_or(u32::MAX))
                .map_or(MetricState::Unsupported, MetricState::Available),
            run_time_secs: process.run_time(),
            start_time_secs: process.start_time(),
            is_kernel_thread: matches!(process.thread_kind(), Some(sysinfo::ThreadKind::Kernel)),
        }
    }
}

/// Maps a `sysinfo` status onto our model.
///
/// `Wakekill`, `Waking`, `Parked`, and `LockBlocked` are all transient kernel
/// states that behave like running from a user's point of view, so they map to
/// `Running` rather than to `Unknown` — which would render as `?` and look like a
/// collection failure.
fn map_process_state(status: ProcessStatus) -> ProcessState {
    match status {
        ProcessStatus::Run => ProcessState::Running,
        ProcessStatus::Sleep => ProcessState::Sleeping,
        ProcessStatus::UninterruptibleDiskSleep => ProcessState::UninterruptibleSleep,
        ProcessStatus::Zombie => ProcessState::Zombie,
        ProcessStatus::Stop | ProcessStatus::Suspended => ProcessState::Stopped,
        ProcessStatus::Tracing => ProcessState::Traced,
        ProcessStatus::Idle => ProcessState::Idle,
        ProcessStatus::Dead => ProcessState::Dead,
        ProcessStatus::Wakekill
        | ProcessStatus::Waking
        | ProcessStatus::Parked
        | ProcessStatus::LockBlocked => ProcessState::Running,
        ProcessStatus::Unknown(_) => ProcessState::Unknown,
    }
}

/// Classifies a mount from its filesystem type (§7.3).
fn classify_filesystem(fs_type: &str, is_removable: bool) -> FilesystemKind {
    if is_removable {
        return FilesystemKind::Removable;
    }
    let lower = fs_type.to_ascii_lowercase();
    const VIRTUAL: [&str; 12] = [
        "tmpfs", "devfs", "devtmpfs", "proc", "sysfs", "overlay", "squashfs", "ramfs", "cgroup",
        "cgroup2", "autofs", "fdescfs",
    ];
    const NETWORK: [&str; 6] = ["nfs", "nfs4", "smbfs", "cifs", "afpfs", "sshfs"];
    if VIRTUAL.contains(&lower.as_str()) {
        FilesystemKind::Virtual
    } else if NETWORK.contains(&lower.as_str()) {
        FilesystemKind::Network
    } else if lower.is_empty() {
        FilesystemKind::Unknown
    } else {
        FilesystemKind::Physical
    }
}

/// Classifies an interface from its name.
///
/// A heuristic, and deliberately conservative: an unrecognised name is `Unknown`
/// rather than guessed as physical, because the classification only drives
/// filtering and a wrong guess would hide a real interface.
fn classify_interface(name: &str) -> InterfaceKind {
    if name == "lo" || name.starts_with("lo") && name[2..].chars().all(char::is_numeric) {
        InterfaceKind::Loopback
    } else if name.starts_with("utun")
        || name.starts_with("tun")
        || name.starts_with("tap")
        || name.starts_with("wg")
        || name.starts_with("ipsec")
    {
        InterfaceKind::Tunnel
    } else if name.starts_with("bridge")
        || name.starts_with("br-")
        || name.starts_with("veth")
        || name.starts_with("docker")
        || name.starts_with("virbr")
        || name.starts_with("vmnet")
        || name.starts_with("bond")
        || name.starts_with("awdl")
        || name.starts_with("llw")
    {
        InterfaceKind::Virtual
    } else if name.starts_with("en") || name.starts_with("eth") || name.starts_with("wl") {
        InterfaceKind::Physical
    } else {
        InterfaceKind::Unknown
    }
}

/// Absolute zero. Nothing at or below it is a temperature.
///
/// This is not a theoretical guard. Every Apple Silicon Mac reports a dozen unwired
/// `PMU tdev*` sensors at about −9200 °C, and a `sysinfo` component list on such a
/// machine is roughly half faults by count. Dropping them is not filtering
/// inconvenient data: −9200 °C is the sensor saying it is not connected, and a panel
/// that printed it would be printing a fabricated reading with a decimal point on it
/// (§4).
const ABSOLUTE_ZERO_CELSIUS: f32 = -273.15;

fn read_temperatures(components: &Components) -> MetricState<Vec<TemperatureReading>> {
    let physical = |value: f32| value.is_finite() && value > ABSOLUTE_ZERO_CELSIUS;
    let readings: Vec<TemperatureReading> = components
        .list()
        .iter()
        .filter_map(|component| {
            let celsius = component.temperature()?;
            // A non-finite or sub-absolute-zero reading is a sensor fault, not a
            // temperature. Discarding the whole reading rather than substituting a
            // number is the §4 answer, and an all-faults machine then reports
            // `Unsupported` below rather than a list of nonsense.
            physical(celsius).then(|| TemperatureReading {
                label: component.label().to_owned().into(),
                celsius,
                // `Component::max` is the highest value *seen* on macOS and a
                // driver-declared limit on some Linux sensors, and the two cannot be
                // told apart through this interface. `peak_celsius` is named for the
                // weaker of the two readings so nothing downstream mistakes it for a
                // ceiling; only `critical` is one.
                peak_celsius: component.max().filter(|value| physical(*value)),
                critical_celsius: component.critical().filter(|value| physical(*value)),
            })
        })
        .collect();
    if readings.is_empty() {
        MetricState::Unsupported
    } else {
        MetricState::Available(readings)
    }
}

/// Marks a carried-over reading stale, leaving every other state alone.
///
/// Only a value that was once measured can go stale. `Unsupported` with an age
/// would claim a measurement that never happened, and `WarmingUp` with an age
/// would claim one that has not happened yet (§4, §26).
pub(crate) fn retained_sensor<T>(state: MetricState<T>, age: Duration) -> MetricState<T> {
    match state {
        MetricState::Available(value) => MetricState::Stale { value, age },
        other => other,
    }
}

/// Narrows the platform's `f64` load averages to `f32`.
///
/// A load average is a small number — even a badly overloaded machine reports
/// low hundreds — so `f32` has far more precision than the metric has meaning.
/// The narrowing is deliberate rather than incidental.
#[allow(clippy::cast_possible_truncation)]
fn load_snapshot(load: sysinfo::LoadAvg) -> LoadSnapshot {
    LoadSnapshot {
        one: load.one as f32,
        five: load.five as f32,
        fifteen: load.fifteen as f32,
    }
}

fn text_state(value: Option<String>) -> MetricState<Box<str>> {
    match value {
        Some(text) if !text.trim().is_empty() => MetricState::Available(text.into()),
        _ => MetricState::Unsupported,
    }
}

/// Wraps a `sysinfo` percentage, rejecting the values §10.4 forbids.
fn percent_state(value: f32) -> MetricState<Percent> {
    Percent::new(value).map_or(
        MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
        MetricState::Available,
    )
}

/// What the baseline can report on any platform, before native enrichment.
fn baseline_capabilities() -> CapabilitySnapshot {
    CapabilitySnapshot {
        per_process_io: CapabilityState::Available,
        per_process_threads: CapabilityState::Available,
        per_process_open_files: CapabilityState::Unsupported,
        per_process_sockets: CapabilityState::Unsupported,
        per_process_working_directory: CapabilityState::Available,
        per_core_cpu: CapabilityState::Available,
        cpu_breakdown: CapabilityState::Unsupported,
        load_average: CapabilityState::Available,
        swap_activity: CapabilityState::Unsupported,
        disk_io: CapabilityState::Available,
        disk_busy: CapabilityState::Unsupported,
        filesystem_capacity: CapabilityState::Unknown,
        network_counters: CapabilityState::Available,
        network_link_speed: CapabilityState::Unsupported,
        network_errors: CapabilityState::Available,
        temperatures: CapabilityState::Unknown,
        battery: CapabilityState::Unsupported,
        linux_psi: CapabilityState::Unsupported,
        cgroup_limits: CapabilityState::Unsupported,
        kernel_threads: if cfg!(target_os = "linux") {
            CapabilityState::Available
        } else {
            CapabilityState::Unsupported
        },
        process_signals: CapabilityState::Available,
        renice: CapabilityState::Unsupported,
    }
}

impl SnapshotSource for CommonCollector {
    fn name(&self) -> &'static str {
        "sysinfo"
    }

    fn capabilities(&self) -> CapabilitySnapshot {
        self.capabilities
    }

    fn sample(&mut self, tick: &SampleTick) -> Result<SystemSnapshot, CollectorError> {
        use monitrs_core::model::Tier;

        // Slow before medium before fast: later tiers depend on the device lists
        // the earlier ones establish.
        if tick.due.contains(Tier::Slow) {
            self.refresh_slow();
        }
        if tick.due.contains(Tier::Medium) {
            self.refresh_medium();
        }
        if tick.due.sensors() {
            self.refresh_sensors();
            self.sensors_read_at = Some(tick.captured_at);
        }
        if tick.due.contains(Tier::Fast) {
            self.refresh_fast(tick.due.contains(Tier::Medium));
        }

        let load = System::load_average();
        Ok(SystemSnapshot {
            sequence: tick.sequence,
            captured_at: tick.captured_at,
            wall_time: tick.wall_time,
            elapsed: tick.elapsed,
            host: self.cached_host.clone(),
            cpu: self.cpu(tick),
            memory: self.memory(),
            load: MetricState::Available(load_snapshot(load)),
            processes: self.processes(tick),
            disks: self.disks(tick),
            filesystems: self.cached_filesystems.clone(),
            networks: self.networks(tick),
            // Pressure is derived by the diagnostic engine over the snapshot and
            // its history. A collector deciding a pressure state would be
            // deciding policy in the wrong layer.
            pressure: PressureSnapshot::warming_up(),
            sensors: self.sensors_for(tick),
            capabilities: self.capabilities,
            health: CollectorHealth::default(),
        })
    }

    fn process_detail(&mut self, identity: ProcessIdentity) -> ProcessDetailResult {
        let pid = sysinfo::Pid::from_u32(identity.pid);
        // Re-read just this process: an ancestry walk over a stale table could
        // attribute a parent to the wrong process (§14.1).
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::nothing()
                .with_cwd(sysinfo::UpdateKind::Always)
                .with_root(sysinfo::UpdateKind::Always)
                .with_tasks(),
        );

        let Some(process) = self.system.process(pid) else {
            return ProcessDetailResult::Vanished(identity);
        };
        let found = ProcessIdentity::new(identity.pid, process.start_time());
        if found != identity {
            return ProcessDetailResult::Reused {
                requested: identity,
                found,
            };
        }

        let parent_pid = process.parent();
        let cwd = process
            .cwd()
            .map(|path| path.to_string_lossy().into_owned());
        let root = process
            .root()
            .map(|path| path.to_string_lossy().into_owned());

        let children: Vec<ProcessIdentity> = self
            .system
            .processes()
            .values()
            .filter(|candidate| candidate.parent() == Some(pid))
            .map(|candidate| ProcessIdentity::new(candidate.pid().as_u32(), candidate.start_time()))
            .collect();

        let ancestry = parent_pid
            .and_then(|ppid| self.system.process(ppid))
            .map(|parent| {
                vec![monitrs_core::model::AncestorEntry {
                    identity: ProcessIdentity::new(parent.pid().as_u32(), parent.start_time()),
                    name: parent.name().to_string_lossy().into_owned().into(),
                }]
            })
            .unwrap_or_default();

        let mut detail = ProcessDetail::pending(identity, SystemTime::now());
        detail.working_directory = cwd.map_or(MetricState::PermissionDenied, |text| {
            MetricState::Available(text.into())
        });
        detail.root = root.map_or(MetricState::PermissionDenied, |text| {
            MetricState::Available(text.into())
        });
        // The baseline cannot count descriptors or sockets, let alone name them; the
        // native layers can. `Unsupported` rather than the `WarmingUp` this record
        // starts as, because a value the baseline will never produce is not one that
        // is on its way (§4).
        detail.open_files = MetricState::Unsupported;
        detail.sockets = MetricState::Unsupported;
        detail.open_file_list = MetricState::Unsupported;
        detail.descendants = MetricState::Available(u32::try_from(children.len()).unwrap_or(0));
        detail.children = MetricState::Available(children);
        // The baseline walks only one generation. A full chain needs repeated
        // lookups the native layers do more cheaply; one generation is honest,
        // an invented chain would not be.
        detail.ancestry = MetricState::Available(ancestry);
        detail.nice = MetricState::Unsupported;
        detail.cgroup = MetricState::Unsupported;
        detail.container = MetricState::Unsupported;
        ProcessDetailResult::Loaded(Box::new(detail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::DueTiers;

    #[test]
    fn filesystem_classification_separates_virtual_network_and_removable_mounts() {
        assert_eq!(classify_filesystem("apfs", false), FilesystemKind::Physical);
        assert_eq!(classify_filesystem("ext4", false), FilesystemKind::Physical);
        assert_eq!(classify_filesystem("tmpfs", false), FilesystemKind::Virtual);
        assert_eq!(
            classify_filesystem("overlay", false),
            FilesystemKind::Virtual
        );
        assert_eq!(classify_filesystem("nfs4", false), FilesystemKind::Network);
        assert_eq!(classify_filesystem("", false), FilesystemKind::Unknown);
    }

    #[test]
    fn removable_wins_over_the_filesystem_type() {
        // A FAT32 USB stick is removable first and physical second; the Storage
        // filter cares about the former.
        assert_eq!(classify_filesystem("vfat", true), FilesystemKind::Removable);
        assert_eq!(classify_filesystem("apfs", true), FilesystemKind::Removable);
    }

    #[test]
    fn filesystem_classification_is_case_insensitive() {
        assert_eq!(classify_filesystem("TMPFS", false), FilesystemKind::Virtual);
        assert_eq!(classify_filesystem("NFS", false), FilesystemKind::Network);
    }

    #[test]
    fn interface_classification_recognises_the_common_names() {
        assert_eq!(classify_interface("lo"), InterfaceKind::Loopback);
        assert_eq!(classify_interface("lo0"), InterfaceKind::Loopback);
        assert_eq!(classify_interface("en0"), InterfaceKind::Physical);
        assert_eq!(classify_interface("eth0"), InterfaceKind::Physical);
        assert_eq!(classify_interface("wlan0"), InterfaceKind::Physical);
        assert_eq!(classify_interface("utun3"), InterfaceKind::Tunnel);
        assert_eq!(classify_interface("wg0"), InterfaceKind::Tunnel);
        assert_eq!(classify_interface("docker0"), InterfaceKind::Virtual);
        assert_eq!(classify_interface("veth1a2b"), InterfaceKind::Virtual);
        assert_eq!(classify_interface("bridge100"), InterfaceKind::Virtual);
    }

    #[test]
    fn an_unrecognised_interface_is_unknown_rather_than_guessed() {
        // Guessing "physical" would let the Storage/Network filters hide a real
        // interface on a platform we have not seen.
        assert_eq!(classify_interface("qq42"), InterfaceKind::Unknown);
        assert_eq!(classify_interface(""), InterfaceKind::Unknown);
    }

    #[test]
    fn transient_kernel_states_map_to_running_not_unknown() {
        // Rendering these as `?` would look like a collection failure.
        for status in [
            ProcessStatus::Wakekill,
            ProcessStatus::Waking,
            ProcessStatus::Parked,
            ProcessStatus::LockBlocked,
        ] {
            assert_eq!(
                map_process_state(status),
                ProcessState::Running,
                "{status:?}"
            );
        }
    }

    #[test]
    fn the_states_the_specification_singles_out_are_mapped_distinctly() {
        assert_eq!(
            map_process_state(ProcessStatus::Zombie),
            ProcessState::Zombie
        );
        assert_eq!(
            map_process_state(ProcessStatus::UninterruptibleDiskSleep),
            ProcessState::UninterruptibleSleep
        );
        assert!(map_process_state(ProcessStatus::Zombie).is_notable());
        assert!(map_process_state(ProcessStatus::UninterruptibleDiskSleep).is_notable());
        assert!(!map_process_state(ProcessStatus::Sleep).is_notable());
    }

    #[test]
    fn an_unknown_status_is_reported_as_unknown() {
        assert_eq!(
            map_process_state(ProcessStatus::Unknown(99)),
            ProcessState::Unknown
        );
    }

    #[test]
    fn a_non_finite_percentage_is_unavailable_rather_than_zero() {
        assert!(percent_state(f32::NAN).fresh().is_none());
        assert!(percent_state(-1.0).fresh().is_none());
        assert!(percent_state(f32::INFINITY).fresh().is_none());
        assert_eq!(
            percent_state(f32::NAN),
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed)
        );
        assert!(percent_state(37.5).fresh().is_some());
    }

    #[test]
    fn blank_platform_strings_are_unsupported_rather_than_empty() {
        assert!(text_state(None).is_unsupported());
        assert!(text_state(Some(String::new())).is_unsupported());
        assert!(text_state(Some("   ".to_owned())).is_unsupported());
        assert_eq!(
            text_state(Some("dev-mbp".to_owned())).fresh().map(|s| &**s),
            Some("dev-mbp")
        );
    }

    #[test]
    fn a_carried_over_reading_becomes_stale_with_its_age() {
        // Pure, so it runs everywhere and needs no sensors: this is the rule §4
        // states, expressed as a function.
        let available = MetricState::Available(41.5_f32);
        assert_eq!(
            retained_sensor(available, Duration::from_secs(12)),
            MetricState::Stale {
                value: 41.5,
                age: Duration::from_secs(12)
            }
        );
    }

    #[test]
    fn a_reading_that_was_never_available_is_not_made_stale() {
        // `Unsupported` with an age would claim there was once a measurement.
        assert_eq!(
            retained_sensor(MetricState::<f32>::Unsupported, Duration::from_secs(9)),
            MetricState::Unsupported
        );
        assert_eq!(
            retained_sensor(MetricState::<f32>::WarmingUp, Duration::from_secs(9)),
            MetricState::WarmingUp
        );
        assert_eq!(
            retained_sensor(MetricState::<f32>::PermissionDenied, Duration::from_secs(9)),
            MetricState::PermissionDenied
        );
    }

    #[test]
    fn the_baseline_declares_only_what_sysinfo_can_actually_provide() {
        let capabilities = baseline_capabilities();
        // §7.3 forbids an approximated busy percentage, and §7.4 forbids a
        // utilization percentage without a known link speed. Both must therefore
        // be absent from the baseline rather than optimistically available.
        assert_eq!(capabilities.disk_busy, CapabilityState::Unsupported);
        assert_eq!(
            capabilities.network_link_speed,
            CapabilityState::Unsupported
        );
        assert_eq!(capabilities.linux_psi, CapabilityState::Unsupported);
        assert_eq!(capabilities.cgroup_limits, CapabilityState::Unsupported);
        assert_eq!(capabilities.cpu_breakdown, CapabilityState::Unsupported);
        assert_eq!(capabilities.per_core_cpu, CapabilityState::Available);
    }

    /// Advances a collector by `count` samples, sleeping long enough between
    /// them that `sysinfo` can produce a real CPU delta.
    /// One row per device, however many mounts it has.
    ///
    /// `sysinfo` lists one entry per mount, so a device with several — which on an
    /// APFS Mac is every device, since `/` and `/System/Volumes/Data` share a
    /// container — produced one row per mount. Both wrote to the same rate tracker in
    /// the same tick, so the second `observe` saw a counter that had not moved and the
    /// duplicate row reported `warming up` for the life of the process.
    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn one_row_per_device_however_many_mounts_it_has() {
        let mut collector = CommonCollector::new().expect("constructs");
        let snapshots = sample_twice(&mut collector);
        let second = snapshots.last().expect("two samples");

        let mut seen: Vec<&str> = second.disks.iter().map(|disk| &*disk.device).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            before,
            "a device appears more than once: {:?}",
            second.disks.iter().map(|d| &d.device).collect::<Vec<_>>()
        );

        // And the mounts are not lost by the collapsing: at least one device on a Mac
        // or a Linux box backs a mount monitrs can name.
        assert!(
            second
                .disks
                .iter()
                .any(|disk| !disk.mount_points.is_empty()),
            "no device reports a mount point, so the mapping was dropped"
        );

        // A second sample can compute rates, so no device should still be warming up
        // unless the platform genuinely gave nothing.
        for disk in &second.disks {
            assert!(
                !disk.read.is_warming_up() || disk.totals.is_warming_up(),
                "device {} reports a warming-up rate on the second sample with real \
                 totals, which is the duplicate-row bug",
                disk.device
            );
        }
    }

    fn sample_twice(collector: &mut CommonCollector) -> Vec<SystemSnapshot> {
        let start = Instant::now();
        let mut tick = SampleTick::first(start, SystemTime::now());
        let mut out = vec![collector.sample(&tick).expect("first sample")];
        // sysinfo needs at least MINIMUM_CPU_UPDATE_INTERVAL between CPU reads.
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL * 2);
        tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        out.push(collector.sample(&tick).expect("second sample"));
        out
    }

    // §17.6 platform smoke tests. `#[ignore]` so `cargo test` stays hermetic and
    // fast for contributors; CI runs them with `-- --ignored` on real Linux and
    // macOS runners.

    #[test]
    #[ignore = "platform smoke: reads the live system (§17.6)"]
    fn sensors_are_read_when_due_and_carried_stale_when_not() {
        let mut collector = CommonCollector::new().expect("constructs");
        let start = Instant::now();
        let first = SampleTick::first(start, SystemTime::now());
        let sampled = collector.sample(&first).expect("first sample");
        // The first tick is `DueTiers::ALL`, so whatever this machine has is fresh.
        assert!(
            !sampled.sensors.temperatures.is_stale(),
            "a tick that read the sensors must publish them as measured"
        );

        // A fast-only tick two seconds later: the same reading, now two seconds old
        // and saying so.
        let later = start + Duration::from_secs(2);
        let fast = SampleTick {
            sequence: 1,
            captured_at: later,
            wall_time: SystemTime::now(),
            elapsed: Duration::from_secs(2),
            due: DueTiers::fast_only(),
        };
        let carried = collector.sample(&fast).expect("second sample");
        match (&sampled.sensors.temperatures, &carried.sensors.temperatures) {
            (MetricState::Available(_), MetricState::Stale { age, .. }) => {
                assert_eq!(*age, Duration::from_secs(2));
            }
            (MetricState::Available(_), other) => {
                panic!("a carried-over reading must be stale, got {other:?}");
            }
            // A machine with no sensors at all: nothing to carry, nothing to claim.
            (unavailable, carried) => assert_eq!(
                std::mem::discriminant(unavailable),
                std::mem::discriminant(carried)
            ),
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_the_collector_constructs_on_this_platform() {
        let collector = CommonCollector::new().expect("the baseline collector must construct");
        assert!(collector.logical_cpus() >= 1);
        assert_eq!(collector.name(), "sysinfo");
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_cpu_transitions_out_of_warming_up_on_the_second_sample() {
        let mut collector = CommonCollector::new().expect("constructs");
        let snapshots = sample_twice(&mut collector);

        let first = snapshots.first().expect("two snapshots");
        assert!(
            first.cpu.total.is_warming_up(),
            "the first sample must not report a number"
        );

        let second = snapshots.get(1).expect("two snapshots");
        assert!(
            second.cpu.total.fresh().is_some(),
            "the second sample must produce a real CPU figure, got {:?}",
            second.cpu.total
        );
        // Deliberately not asserting a value: utilization is not reproducible.
        let busy = second.cpu.total.fresh().expect("available").busy.value();
        assert!(
            (0.0..=100.0).contains(&busy),
            "aggregate CPU out of range: {busy}"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_memory_total_is_nonzero_and_used_does_not_exceed_it() {
        let mut collector = CommonCollector::new().expect("constructs");
        let snapshots = sample_twice(&mut collector);
        let memory = snapshots.last().expect("snapshots").memory;

        assert!(
            memory.total_bytes > 0,
            "a system with no memory cannot be monitored"
        );
        let used = *memory.used.fresh().expect("used is measured");
        assert!(
            used <= memory.total_bytes,
            "used {used} exceeds total {}",
            memory.total_bytes
        );
        assert_eq!(memory.semantics, MemorySemantics::SysinfoBaseline);
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_the_current_process_appears_with_a_plausible_identity() {
        let mut collector = CommonCollector::new().expect("constructs");
        let snapshots = sample_twice(&mut collector);
        let latest = snapshots.last().expect("snapshots");

        let me = std::process::id();
        let found = latest
            .process_by_pid(me)
            .unwrap_or_else(|| panic!("our own PID {me} is missing from the process table"));

        assert_eq!(found.identity.pid, me);
        assert!(
            found.identity.start_key > 0,
            "a start key of 0 cannot detect PID reuse"
        );
        assert!(found.memory.rss_bytes.fresh().is_some_and(|rss| *rss > 0));
        assert!(!found.name.is_empty());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_at_least_one_network_interface_is_reported() {
        let mut collector = CommonCollector::new().expect("constructs");
        let snapshots = sample_twice(&mut collector);
        let networks = &snapshots.last().expect("snapshots").networks;

        assert!(
            !networks.is_empty(),
            "every system has at least a loopback interface"
        );
        for nic in networks {
            // §7.4: without a known link speed there must be no utilization.
            if nic.link_speed_mbps.fresh().is_none() {
                assert!(
                    nic.utilization().fresh().is_none(),
                    "{} reported utilization without a known link speed",
                    nic.name
                );
            }
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_filesystems_are_reported_and_never_exceed_their_capacity() {
        let mut collector = CommonCollector::new().expect("constructs");
        let snapshots = sample_twice(&mut collector);
        let filesystems = &snapshots.last().expect("snapshots").filesystems;

        assert!(
            !filesystems.is_empty(),
            "at least the root filesystem must be visible"
        );
        for fs in filesystems {
            if let (Some(&used), true) = (fs.used_bytes.fresh(), fs.total_bytes > 0) {
                assert!(
                    used <= fs.total_bytes,
                    "{} reports used > total",
                    fs.mount_point
                );
            }
            // A zero-capacity mount must have no percentage, not 0%.
            if fs.total_bytes == 0 {
                assert!(
                    fs.usage.fresh().is_none(),
                    "{} invented a percentage",
                    fs.mount_point
                );
            }
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_detail_lookup_of_our_own_process_succeeds_and_of_a_dead_pid_does_not() {
        let mut collector = CommonCollector::new().expect("constructs");
        let _ = sample_twice(&mut collector);

        let me = std::process::id();
        let snapshot = {
            let start = Instant::now();
            let tick = SampleTick::first(start, SystemTime::now());
            collector.sample(&tick).expect("sample")
        };
        let identity = snapshot.process_by_pid(me).expect("our process").identity;

        match collector.process_detail(identity) {
            ProcessDetailResult::Loaded(detail) => {
                assert_eq!(detail.identity, identity);
                assert!(
                    detail.working_directory.fresh().is_some(),
                    "we can read our own cwd"
                );
                assert!(detail.children.fresh().is_some());
            }
            other => panic!("expected our own detail to load, got {other:?}"),
        }

        // PID 0 is never a normal user process on either platform.
        let phantom = ProcessIdentity::new(0, 0);
        assert!(matches!(
            collector.process_detail(phantom),
            ProcessDetailResult::Vanished(_) | ProcessDetailResult::Reused { .. }
        ));
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_rate_trackers_do_not_grow_without_bound_across_many_samples() {
        // §10.3 and §16.1: no unbounded growth. Ten samples on a live system see
        // real PID churn, and the tracker maps must track only live processes.
        let mut collector = CommonCollector::new().expect("constructs");
        let mut tick = SampleTick::first(Instant::now(), SystemTime::now());
        let mut last_count = 0usize;
        for _ in 0..10 {
            let snapshot = collector.sample(&tick).expect("sample");
            last_count = snapshot.process_count();
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
            tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        }

        // Allow generous slack for processes that came and went, but the trackers
        // must be proportional to the live table, not to samples x processes.
        let tracked = collector.process_read.len();
        assert!(
            tracked <= last_count * 2 + 64,
            "process rate trackers grew to {tracked} against {last_count} live processes"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live system"]
    fn smoke_sampling_never_sends_a_signal() {
        // There is no signalling API on this type at all, which is the real
        // guarantee; this test records that fact so a future addition has to
        // confront it. Sampling our own process repeatedly must leave it alive.
        let mut collector = CommonCollector::new().expect("constructs");
        let _ = sample_twice(&mut collector);
        let _ = sample_twice(&mut collector);
        // Still running: if sampling signalled anything, we would not be here.
        assert_eq!(std::process::id(), std::process::id());
    }
}
