//! JSON snapshot export.
//!
//! Export is always explicit — there is no automatic upload, no telemetry, and no
//! background write (§15.2). What it produces is meant to be pasted into a bug
//! report, which is why the redaction defaults are conservative:
//!
//! * **Environment variable values cannot appear**, because
//!   [`monitrs_core::model::ProcessDetail`] has no field for them and monitrs never
//!   reads them (§7.5).
//! * **Command arguments are redacted by default.** A `psql
//!   postgres://user:hunter2@host/db` command line in a public issue is a leaked
//!   credential, and people paste exports without reading them.
//! * **Open-file paths cannot appear at all**, redacted or otherwise. There are two
//!   reasons and they compound: this projection has no [`ProcessDetail`] in it —
//!   §8.6 loads the detail for one selected process, which is not part of a
//!   *snapshot* — and [`ProcessDetail`]'s own `Serialize` implementation replaces a
//!   descriptor's path with its availability state whatever serializes it. So there
//!   is no `--include-paths` to add by accident, and an export that starts including
//!   the detail cannot leak `/Users/someone/Documents/…` by forgetting to redact
//!   (§15.2, §19).
//!
//! [`ProcessDetail`]: monitrs_core::model::ProcessDetail
//! * **Unavailable metrics export as a named state**, never as `null` or `0`, so
//!   the export cannot mislead a reader the way the UI is forbidden from
//!   misleading a user (§4).
//!
//! [`monitrs_core::SystemSnapshot`] is deliberately not `Serialize`: it holds an
//! [`std::time::Instant`], which has no meaningful serialized form. This module is
//! the projection that replaces it with wall-clock time.

use std::time::{Duration, SystemTime};

use monitrs_core::SystemSnapshot;
use monitrs_core::model::{
    CapabilitySnapshot, CollectorHealth, CpuSnapshot, DiskSnapshot, FilesystemSnapshot,
    HostSnapshot, LoadSnapshot, MemorySnapshot, MetricState, NetworkSnapshot, PressureSnapshot,
    ProcessIdentity, ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState, SensorSnapshot,
    UserIdentity,
};
use serde::Serialize;

/// The export format version.
///
/// Bumped whenever a field is removed or its meaning changes, so a consumer can
/// refuse an export it does not understand rather than misread it.
///
/// `2` since 0.2.0. Two fields were **removed** — `sensors.battery.health` and
/// `sensors.temperatures[].high_celsius` — and the second's replacement,
/// `peak_celsius`, deliberately means something different: the old name claimed a
/// declared threshold and the value was a high-water mark. A script that read
/// `high_celsius` as a limit would misread `peak_celsius` as one, which is exactly
/// what this constant exists to prevent. Deriving the old `health` needs the two
/// figures in `sensors.battery.capacity`: `full_microwatt_hours / design_microwatt_hours`.
///
/// Fields were also added, which alone would not have justified a bump: `cpu.cgroup_quota`,
/// `cpu.core_classes`, `memory.cgroup_used_bytes`, `filesystems[].inodes`,
/// `sensors.battery.{capacity, temperature_celsius, power_watts}`, and
/// `host.environment.available.container` on Linux.
pub(crate) const SCHEMA_VERSION: u32 = 2;

/// What may appear in an export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RedactionPolicy {
    /// Whether to strip process arguments, keeping only the program.
    ///
    /// `true` by default. Turning it off is an explicit `--include-arguments`.
    pub(crate) redact_arguments: bool,
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        Self {
            redact_arguments: true,
        }
    }
}

impl RedactionPolicy {
    /// The conservative default: arguments removed.
    pub(crate) const REDACTED: Self = Self {
        redact_arguments: true,
    };

    /// Full command lines included, at the caller's explicit request.
    pub(crate) const FULL: Self = Self {
        redact_arguments: false,
    };
}

/// What produced the export, so a report identifies its own tool version.
#[derive(Debug, Serialize)]
pub(crate) struct ToolInfo {
    name: &'static str,
    version: &'static str,
}

/// A machine-readable record of what was withheld.
///
/// Present even when nothing was redacted, so a consumer never has to infer
/// whether a field is absent or merely empty.
#[derive(Debug, Serialize)]
pub(crate) struct RedactionInfo {
    /// Whether process arguments were stripped.
    arguments_redacted: bool,
    /// Always `true`: monitrs does not read environment variables at all.
    environment_excluded: bool,
    /// A human-readable note for whoever reads the file.
    note: &'static str,
}

/// A wall-clock instant, in both a machine and a human form.
#[derive(Debug, Serialize)]
pub(crate) struct Timestamp {
    /// Seconds since the Unix epoch.
    unix_seconds: i64,
    /// Nanosecond part.
    nanoseconds: u32,
    /// UTC in RFC 3339 form, e.g. `2026-07-29T20:14:07Z`.
    utc: String,
}

impl Timestamp {
    fn from_system_time(time: SystemTime) -> Self {
        let (unix_seconds, nanoseconds) = match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(since) => (
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
                since.subsec_nanos(),
            ),
            // A clock set before 1970 is unusual but must not panic.
            Err(error) => {
                let before = error.duration();
                (
                    i64::try_from(before.as_secs()).map_or(i64::MIN, |secs| -secs),
                    before.subsec_nanos(),
                )
            }
        };
        Self {
            unix_seconds,
            nanoseconds,
            utc: format_rfc3339(unix_seconds),
        }
    }
}

/// Formats Unix seconds as RFC 3339 UTC.
///
/// Implemented locally rather than by adding a date library: this is the only
/// place monitrs needs calendar arithmetic, and §13 asks for a narrowly
/// implemented parser over a dependency when the scope is this small. The
/// days-to-civil conversion is the standard shift-the-epoch-to-March algorithm,
/// which handles leap years and centuries without a lookup table.
fn format_rfc3339(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let seconds_of_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    // Shift the epoch from 1970-01-01 to 0000-03-01 so that the leap day lands at
    // the end of the cycle and needs no special case.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * shifted_month + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    })
    .unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// One process, with the redaction policy applied.
#[derive(Debug, Serialize)]
pub(crate) struct ProcessExport<'a> {
    identity: ProcessIdentity,
    parent_pid: Option<u32>,
    name: &'a str,
    /// Either the full command line or just the program, per the policy.
    command: &'a str,
    /// Whether `command` had arguments removed.
    command_redacted: bool,
    exe: Option<&'a str>,
    user: &'a MetricState<UserIdentity>,
    state: ProcessState,
    state_code: char,
    cpu_percent: &'a MetricState<monitrs_core::units::Percent>,
    memory: &'a ProcessMemory,
    io: &'a ProcessIo,
    threads: &'a MetricState<u32>,
    age: &'a MetricState<Duration>,
    is_kernel_thread: bool,
}

impl<'a> ProcessExport<'a> {
    fn new(process: &'a ProcessSnapshot, policy: RedactionPolicy) -> Self {
        let command = if policy.redact_arguments {
            process.redacted_command()
        } else {
            process.command_or_name()
        };
        Self {
            identity: process.identity,
            parent_pid: process.parent_pid,
            name: &process.name,
            command,
            command_redacted: policy.redact_arguments,
            exe: process.exe.as_deref(),
            user: &process.user,
            state: process.state,
            state_code: process.state.code(),
            cpu_percent: &process.cpu,
            memory: &process.memory,
            io: &process.io,
            threads: &process.threads,
            age: &process.age,
            is_kernel_thread: process.is_kernel_thread,
        }
    }
}

/// A serializable projection of one snapshot.
///
/// Borrows from the snapshot rather than cloning it: an export of a
/// 10,000-process table should not double the program's memory use.
#[derive(Debug, Serialize)]
pub(crate) struct SnapshotExport<'a> {
    schema_version: u32,
    tool: ToolInfo,
    redaction: RedactionInfo,
    sequence: u64,
    wall_time: Timestamp,
    /// The measured interval since the previous snapshot, in milliseconds.
    ///
    /// Zero means this is the first snapshot, in which case every delta-based
    /// metric is `warming_up` rather than zero (§8.2).
    elapsed_millis: u128,
    host: &'a HostSnapshot,
    cpu: &'a CpuSnapshot,
    memory: &'a MemorySnapshot,
    load: &'a MetricState<LoadSnapshot>,
    process_count: usize,
    processes: Vec<ProcessExport<'a>>,
    disks: &'a [DiskSnapshot],
    filesystems: &'a [FilesystemSnapshot],
    networks: &'a [NetworkSnapshot],
    pressure: &'a PressureSnapshot,
    sensors: &'a SensorSnapshot,
    capabilities: &'a CapabilitySnapshot,
    health: &'a CollectorHealth,
}

impl<'a> SnapshotExport<'a> {
    /// Projects `snapshot` for export under `policy`.
    pub(crate) fn new(snapshot: &'a SystemSnapshot, policy: RedactionPolicy) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            tool: ToolInfo {
                name: env!("CARGO_PKG_NAME"),
                version: env!("CARGO_PKG_VERSION"),
            },
            redaction: RedactionInfo {
                arguments_redacted: policy.redact_arguments,
                environment_excluded: true,
                note: if policy.redact_arguments {
                    "process arguments were removed because they can contain credentials; \
                     environment variable values are never read by monitrs"
                } else {
                    "full command lines are included at the caller's explicit request and may \
                     contain credentials; environment variable values are never read by monitrs"
                },
            },
            sequence: snapshot.sequence,
            wall_time: Timestamp::from_system_time(snapshot.wall_time),
            elapsed_millis: snapshot.elapsed.as_millis(),
            host: &snapshot.host,
            cpu: &snapshot.cpu,
            memory: &snapshot.memory,
            load: &snapshot.load,
            process_count: snapshot.process_count(),
            processes: snapshot
                .processes
                .iter()
                .map(|process| ProcessExport::new(process, policy))
                .collect(),
            disks: &snapshot.disks,
            filesystems: &snapshot.filesystems,
            networks: &snapshot.networks,
            pressure: &snapshot.pressure,
            sensors: &snapshot.sensors,
            capabilities: &snapshot.capabilities,
            health: &snapshot.health,
        }
    }

    /// Renders indented JSON, which is what a human pastes into an issue.
    pub(crate) fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitrs_collectors::fake::{FakeProcess, Pattern, Scenario};
    use monitrs_collectors::{FakeCollector, SampleTick, SnapshotSource as _};
    use monitrs_core::model::{OpenFileEntry, OpenFileKind, OpenFileList, ProcessDetail};
    use std::time::Instant;

    fn snapshot_from(scenario: Scenario, samples: u64) -> SystemSnapshot {
        let mut collector = FakeCollector::new(scenario);
        let start = Instant::now();
        let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut latest = collector.sample(&tick).expect("first sample");
        for index in 1..samples {
            tick = tick.advance(
                start + Duration::from_secs(index),
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_100_447 + index),
                monitrs_collectors::DueTiers::ALL,
            );
            latest = collector.sample(&tick).expect("sample");
        }
        latest
    }

    fn secret_scenario() -> Scenario {
        Scenario {
            processes: vec![
                FakeProcess::new(
                    4_242,
                    500_100,
                    "psql",
                    "psql postgres://admin:hunter2@db.internal/prod",
                )
                .with_cpu(Pattern::Steady(3.0)),
            ],
            ..Scenario::default()
        }
    }

    #[test]
    fn arguments_are_redacted_by_default() {
        let snapshot = snapshot_from(secret_scenario(), 2);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::default())
            .to_json()
            .expect("serializes");

        assert!(!json.contains("hunter2"), "a credential reached the export");
        assert!(
            !json.contains("postgres://"),
            "a connection string reached the export"
        );
        assert!(
            json.contains("psql"),
            "the program itself should still be identifiable"
        );
        assert!(json.contains("\"arguments_redacted\": true"));
    }

    #[test]
    fn full_command_lines_require_an_explicit_opt_in() {
        let snapshot = snapshot_from(secret_scenario(), 2);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::FULL)
            .to_json()
            .expect("serializes");

        assert!(
            json.contains("hunter2"),
            "opting in should include arguments"
        );
        assert!(json.contains("\"arguments_redacted\": false"));
        assert!(
            json.contains("may contain credentials"),
            "an opted-in export must warn the reader"
        );
    }

    #[test]
    fn the_redaction_default_is_the_conservative_one() {
        assert_eq!(RedactionPolicy::default(), RedactionPolicy::REDACTED);
        assert!(RedactionPolicy::default().redact_arguments);
    }

    #[test]
    fn environment_values_are_always_excluded() {
        let snapshot = snapshot_from(Scenario::default(), 3);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::FULL)
            .to_json()
            .expect("serializes");
        assert!(json.contains("\"environment_excluded\": true"));
        // The model has no field for them, so even the permissive policy cannot
        // leak one.
        assert!(!json.contains("\"environ\""));
    }

    #[test]
    fn a_snapshot_export_carries_no_open_file_paths_because_it_carries_no_detail() {
        // §8.6's detail is per-selected-process and is not part of a snapshot, so the
        // paths §7.2 puts on screen have no route into a file someone pastes into a
        // public issue. This test is what makes that a decision rather than an
        // accident: adding a `detail` field here would fail it.
        let snapshot = snapshot_from(Scenario::default(), 3);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::FULL)
            .to_json()
            .expect("serializes");
        // `per_process_open_files` is a *capability* and does belong here; what must
        // not appear is the listing itself or anything out of it.
        for absent in [
            "open_file_list",
            "descriptor",
            "/dev/null",
            "libmonitrs_core.rlib",
        ] {
            assert!(!json.contains(absent), "{absent} reached the export");
        }
    }

    #[test]
    fn a_descriptor_path_cannot_reach_a_serializer_even_when_one_is_pointed_at_it() {
        // The second half of the same rule, and the one that survives a future export
        // deciding to include the detail: `ProcessDetail` serializes a descriptor's
        // path as its *state* and never as its text (§15.2, §19). The state is §4's
        // information and must survive; the path is the user's and must not.
        let mut detail = ProcessDetail::pending(
            ProcessIdentity::new(31_842, 900_100),
            SystemTime::UNIX_EPOCH,
        );
        detail.open_file_list = MetricState::Available(OpenFileList::listed(
            vec![
                OpenFileEntry {
                    descriptor: 3,
                    kind: OpenFileKind::File,
                    path: MetricState::Available(
                        "/Users/gabor/Documents/tax-return-2025.pdf".into(),
                    ),
                },
                OpenFileEntry {
                    descriptor: 4,
                    kind: OpenFileKind::Socket,
                    path: MetricState::Unsupported,
                },
                OpenFileEntry {
                    descriptor: 5,
                    kind: OpenFileKind::File,
                    path: MetricState::PermissionDenied,
                },
            ],
            9,
        ));
        let json = serde_json::to_string(&detail).expect("serializes");

        assert!(!json.contains("tax-return"), "a path reached a serializer");
        assert!(!json.contains("/Users/"), "a path reached a serializer");
        assert!(json.contains("redacted"), "the field must still be present");
        // §4's states are not the user's data and stay legible.
        assert!(json.contains("permission_denied"), "{json}");
        assert!(json.contains("unsupported"), "{json}");
        assert!(json.contains("socket"), "the kind is not user data");
    }

    #[test]
    fn unavailable_metrics_export_as_a_named_state_not_null_or_zero() {
        let snapshot = snapshot_from(Scenario::permission_denied(), 3);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::default())
            .to_json()
            .expect("serializes");

        assert!(
            json.contains("permission_denied"),
            "denial must be visible in the export"
        );
        assert!(
            json.contains("unsupported"),
            "unsupported metrics must be named"
        );
        // The real invariant: a *metric* never exports as `null` and never as a
        // bare number when it was not measured. `null` is reserved for absent
        // optional identifiers — a disk with no model string, a process with no
        // parent — which are not measurements at all.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let cpu = parsed.pointer("/cpu/total").expect("cpu.total is present");
        assert!(
            cpu.is_string() || cpu.is_object(),
            "an unavailable CPU must serialize as a named state, got {cpu}"
        );

        for (index, process) in parsed
            .pointer("/processes")
            .and_then(serde_json::Value::as_array)
            .expect("processes array")
            .iter()
            .enumerate()
        {
            let io_read = process.pointer("/io/read").expect("io.read is present");
            assert!(
                io_read.is_string() || io_read.is_object(),
                "process {index} io.read is {io_read}, not a named state"
            );
        }
    }

    #[test]
    fn the_first_snapshot_exports_as_warming_up_rather_than_zero() {
        let snapshot = snapshot_from(Scenario::default(), 1);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::default())
            .to_json()
            .expect("serializes");

        assert!(json.contains("warming_up"));
        assert!(json.contains("\"elapsed_millis\": 0"));
        assert!(json.contains("\"sequence\": 0"));
    }

    #[test]
    fn the_export_identifies_its_schema_and_tool_version() {
        let snapshot = snapshot_from(Scenario::default(), 2);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::default())
            .to_json()
            .expect("serializes");
        assert!(json.contains("\"schema_version\": 2"));
        assert!(json.contains(env!("CARGO_PKG_VERSION")));
        assert!(json.contains("monitrs"));
    }

    #[test]
    fn the_export_is_valid_json_and_round_trips_through_a_parser() {
        let snapshot = snapshot_from(Scenario::default(), 4);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::default())
            .to_json()
            .expect("serializes");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        assert_eq!(
            parsed
                .get("schema_version")
                .and_then(serde_json::Value::as_u64),
            Some(u64::from(SCHEMA_VERSION)),
            "the parsed version must be the declared one, so a bump cannot be half-applied"
        );
        assert!(
            parsed
                .get("processes")
                .is_some_and(serde_json::Value::is_array)
        );
        assert_eq!(
            parsed
                .get("process_count")
                .and_then(serde_json::Value::as_u64),
            Some(snapshot.process_count() as u64)
        );
    }

    #[test]
    fn wall_time_is_exported_in_both_machine_and_human_form() {
        let snapshot = snapshot_from(Scenario::default(), 2);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::default())
            .to_json()
            .expect("serializes");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let wall = parsed.get("wall_time").expect("wall_time present");

        assert!(
            wall.get("unix_seconds")
                .is_some_and(serde_json::Value::is_i64)
        );
        let utc = wall
            .get("utc")
            .and_then(serde_json::Value::as_str)
            .expect("utc string");
        assert!(utc.ends_with('Z'), "{utc}");
        assert_eq!(utc.len(), 20, "{utc} should be RFC 3339 to the second");
    }

    #[test]
    fn rfc3339_formatting_matches_known_instants() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1), "1970-01-01T00:00:01Z");
        // 2000-02-29 exists: a leap year divisible by 400.
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        // 1900 was not a leap year; 1900-03-01 follows 1900-02-28.
        assert_eq!(format_rfc3339(-2_203_891_200), "1900-03-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_785_100_447), "2026-07-26T21:14:07Z");
        assert_eq!(format_rfc3339(2_147_483_647), "2038-01-19T03:14:07Z");
    }

    #[test]
    fn a_pre_epoch_clock_does_not_panic() {
        let before = SystemTime::UNIX_EPOCH - Duration::from_secs(86_400);
        let stamp = Timestamp::from_system_time(before);
        assert_eq!(stamp.unix_seconds, -86_400);
        assert_eq!(stamp.utc, "1969-12-31T00:00:00Z");
    }

    #[test]
    fn exporting_a_large_table_borrows_rather_than_cloning() {
        // Not a memory assertion, but it does pin the borrowing API: if
        // `SnapshotExport` ever started owning its data, this would stop
        // compiling because the snapshot is still usable afterwards.
        let snapshot = snapshot_from(Scenario::with_process_count(2_000), 2);
        let export = SnapshotExport::new(&snapshot, RedactionPolicy::default());
        assert_eq!(export.process_count, 2_000);
        assert_eq!(snapshot.process_count(), 2_000);
    }

    #[test]
    fn every_process_state_exports_its_single_letter_code() {
        let scenario = Scenario {
            processes: vec![
                FakeProcess::new(1, 1, "z", "z").with_state(ProcessState::Zombie),
                FakeProcess::new(2, 2, "d", "d").with_state(ProcessState::UninterruptibleSleep),
            ],
            ..Scenario::default()
        };
        let snapshot = snapshot_from(scenario, 2);
        let json = SnapshotExport::new(&snapshot, RedactionPolicy::default())
            .to_json()
            .expect("serializes");
        assert!(json.contains("\"state_code\": \"Z\""));
        assert!(json.contains("\"state_code\": \"D\""));
        assert!(json.contains("zombie"));
    }
}
