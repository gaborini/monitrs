//! The JSON export's field paths are a contract (§15.2, and the design
//! document's B1).
//!
//! `README.md` promises that `schema_version` is bumped whenever a field is
//! **removed** or its meaning changes. Nothing checked that. This does: every
//! field path in `docs/schema/v<N>.json` must still be produced by the exporter.
//! Adding a path is fine — a consumer that reads by name cannot be broken by a
//! field it does not know about. Removing or renaming one is a version bump, and
//! the failure message says so.
//!
//! # Why the module source is included rather than imported
//!
//! `monitrs` is a binary-only crate — `crates/monitrs/Cargo.toml` declares a
//! `[[bin]]` and no `[lib]` — so there is no library target for this test to link
//! against, and `export.rs`'s `SnapshotExport` and `SCHEMA_VERSION` are
//! deliberately `pub(crate)`: 1.0.0 freezes the public API, and a contract test is
//! not a reason to widen it. `#[path]` puts the real module into this test binary
//! instead, exactly as `crates/monitrs/tests/integration.rs:90-101` and
//! `crates/monitrs/tests/soak.rs:44` already do for the same reason. Only
//! `export.rs` is included here — it depends on nothing else under `src/`, unlike
//! `config.rs` (which needs `cli.rs`) or `runtime.rs` (which needs `logging.rs`) —
//! so this is the minimum that compiles. One side effect, shared with those two
//! files: `cargo test` sets `cfg(test)` for an integration target too, so
//! `export.rs`'s own `mod tests` is compiled into this binary and runs again here
//! under `export::tests::*`.
//!
//! # How the inventory is generated
//!
//! From [`monitrs_collectors::FakeCollector`], not the live machine: a real host
//! has no battery, or no cgroup, or no inode counts, and an absent collection
//! contributes no field paths at all — an inventory generated from a thin
//! snapshot would silently promise less than the schema actually has. No single
//! named scenario in `fake.rs` populates everything the schema can carry, so
//! [`richest_snapshot`] builds one by hand from `Scenario`'s public fields and
//! patches in the handful of things no scenario (and no collector) can produce:
//! see its doc comment for exactly which, and why.
//!
//! # `Stale` is a second wire shape, not a lesser `Available`
//!
//! [`MetricState`] uses serde's default enum representation, so `Available(T)`
//! and `Stale { value: T, age }` are two *different* JSON shapes for the same
//! field: `{"available": T}` versus `{"stale": {"value": T, "age": ..}}`. A
//! snapshot in which every metric is either fresh or one of the always-absent
//! unit variants — which is what [`richest_snapshot`] alone would produce — never
//! exercises the second shape, so no `*.stale.*` path would ever reach the
//! inventory even though Tasks 3 and 4 of this branch taught the collectors to
//! publish exactly that shape for a carried-over sensor reading, and
//! `crates/monitrs-core/src/model/memory.rs:205-212` documents a transient
//! cgroup-limit read producing it too, as an ordinary occurrence rather than an
//! edge case. [`sensors_gone_stale_export_json`] forces the sensor group — the
//! battery *and* the temperatures — into `Stale` on a clone of the same snapshot,
//! specifically so that no `#[serde(rename)]` on `Stale`'s own fields (a change
//! `cargo-semver-checks` cannot see, since the Rust field name would be untouched)
//! can silently change every stale metric's shape without this guard noticing.
//!
//! Which fields get aged is not cosmetic, and the 1.0.0 review is the evidence: this
//! aged only `sensors.battery`, so `sensors.temperatures.stale.*` was missing from
//! the inventory while being the shape the palette's `:export` actually emits at
//! idle. A guard that ages an arbitrary subset of a group whose members move
//! together inventories a shape the program does not emit and misses the one it
//! does. The rule this now follows: age whatever the collectors age together.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              failure must name the line that broke"
)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use monitrs_collectors::{DueTiers, FakeCollector, SampleTick, Scenario, SnapshotSource as _};
use monitrs_core::SystemSnapshot;
use monitrs_core::diagnostics::{PressureEngine, Thresholds};
use monitrs_core::model::{
    CollectorHealth, CollectorIssue, CpuBreakdown, MetricState, PsiResource, PsiSnapshot,
    SelfOverhead, TierHealth,
};
use monitrs_core::units::Percent;

#[path = "../src/export.rs"]
mod export;

use export::{RedactionPolicy, SCHEMA_VERSION, SnapshotExport};

/// Every leaf path in a JSON document, as `a.b[].c`.
///
/// Array indices collapse to `[]`: the contract is about the shape of an element,
/// and a machine with three disks would otherwise have a different schema from one
/// with four.
fn field_paths(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                field_paths(child, &path, out);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            // An empty array on this machine says nothing about the element's
            // shape, which is why the inventory is generated from a fake snapshot
            // with every collection populated.
            for item in items {
                field_paths(item, &path, out);
            }
        }
        _ => {
            out.insert(prefix.to_owned());
        }
    }
}

/// Where the field-path inventory for schema version `version` lives.
fn inventory_path(version: u32) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/schema")
        .join(format!("v{version}.json"))
}

/// The scenario `fake.rs` does not offer under any single name.
///
/// `Scenario::default()` already carries the reference workload's battery,
/// temperatures, and readable per-process I/O and descriptors. It is missing two
/// things an export can carry that no named scenario adds on top of it:
///
/// * A cgroup limit — without `cgroup_limit_bytes`, `cpu.cgroup_quota` is always
///   `unsupported` and `host.environment.available.container` is always `null`;
///   `Scenario::containerised()` sets it but starts from `Scenario::default()`
///   too, so using it instead would have been equivalent to writing this out.
/// * A known network link speed — without it, §7.4 forbids a utilization
///   percentage at all, so `networks[].link_speed_mbps` could only ever render as
///   `temporarily_unavailable`, never as a number.
fn richest_scenario() -> Scenario {
    Scenario {
        cgroup_limit_bytes: Some(2 * 1024 * 1024 * 1024),
        link_speed_mbps: Some(1_000),
        ..Scenario::default()
    }
}

/// A representative Linux `/proc/pressure/*` reading.
///
/// No collector in this fake ever produces one: §2.3's ownership boundary keeps
/// collectors from deriving pressure, and `FakeCollector::sensors`/`cpu`/etc. have
/// no Linux code path to begin with, so `pressure.psi` is `unsupported` on every
/// scenario. Left alone, the inventory would never see `pressure.psi.available.*`
/// even though a real Linux host populates it every tick.
fn fake_psi_snapshot() -> PsiSnapshot {
    let resource = |total_stalled_secs: u64| PsiResource {
        some_avg10: Percent::new(4.2).unwrap_or(Percent::ZERO),
        some_avg60: Percent::new(3.1).unwrap_or(Percent::ZERO),
        some_avg300: Percent::new(2.0).unwrap_or(Percent::ZERO),
        full_avg10: MetricState::Available(Percent::new(1.0).unwrap_or(Percent::ZERO)),
        full_avg60: MetricState::Available(Percent::new(0.5).unwrap_or(Percent::ZERO)),
        full_avg300: MetricState::Available(Percent::new(0.2).unwrap_or(Percent::ZERO)),
        total_stalled: Duration::from_secs(total_stalled_secs),
    };
    PsiSnapshot {
        cpu: resource(120),
        memory: resource(45),
        io: resource(10),
    }
}

/// A representative `/proc/stat`-style CPU time breakdown.
///
/// `FakeCollector::cpu` builds every [`monitrs_core::model::CpuUsage`] with
/// `CpuUsage::plain`, whose `breakdown` is hard-coded `Unsupported` — there is no
/// scenario knob for it, because the fake platform has no Linux path. On Linux
/// this is always populated, so without this patch the inventory would be
/// missing `cpu.total.breakdown.*` and `cpu.per_core[].breakdown.*` entirely.
fn fake_cpu_breakdown() -> CpuBreakdown {
    CpuBreakdown {
        user: Percent::new(38.0).unwrap_or(Percent::ZERO),
        system: Percent::new(12.0).unwrap_or(Percent::ZERO),
        nice: Percent::new(0.5).unwrap_or(Percent::ZERO),
        idle: Percent::new(49.5).unwrap_or(Percent::ZERO),
        iowait: MetricState::Available(Percent::new(3.0).unwrap_or(Percent::ZERO)),
        irq: MetricState::Available(Percent::new(0.2).unwrap_or(Percent::ZERO)),
        softirq: MetricState::Available(Percent::new(0.1).unwrap_or(Percent::ZERO)),
        steal: MetricState::Available(Percent::new(0.0).unwrap_or(Percent::ZERO)),
    }
}

/// A collector health record with both an overhead measurement and a retained
/// issue.
///
/// `FakeCollector::sample` always returns `CollectorHealth::default()`: every
/// `TierHealth` is zeroed, `issues` is empty, and `self_overhead` is `None`. An
/// empty `Vec` contributes no field paths at all — the same reasoning §4 applies
/// to an absent collection — so `health.issues[].*` would never appear, and
/// `self_overhead` being `None` means `health.self_overhead.*` would never appear
/// either, even though both are ordinary fields of a monitrs that has been
/// running for a while.
fn rich_health() -> CollectorHealth {
    let tier = || TierHealth {
        last_duration: Duration::from_millis(4),
        max_duration: Duration::from_millis(11),
        p95_duration: Duration::from_millis(8),
        completed: 1_200,
        failed: 2,
        since_last: Some(Duration::from_millis(950)),
    };
    CollectorHealth {
        fast: tier(),
        medium: tier(),
        slow: tier(),
        on_demand: tier(),
        dropped_samples: 0,
        coalesced_samples: 3,
        lag: Duration::from_millis(120),
        issues: vec![CollectorIssue {
            source: "/proc/diskstats".into(),
            message: "read failed".into(),
            occurrences: 4,
            last_seen: Some(Duration::from_secs(30)),
        }],
        self_overhead: Some(SelfOverhead {
            cpu: Percent::new(0.6).unwrap_or(Percent::ZERO),
            rss_bytes: 28 * 1024 * 1024,
            history_bytes: 512 * 1024,
            open_files: MetricState::Available(42),
        }),
    }
}

/// How many samples to drive through the fake collector before exporting.
///
/// [`Thresholds::default`]'s `sustained_window` is 15 (§12): a pressure signal
/// cannot settle out of `warming_up` until it has that many observations, and
/// `held_for` — which carries its own `Duration` substructure, a field path nothing
/// else in this export exercises — stays `None` until it does. This runs
/// comfortably past that so the steady signals (memory, swap, load all use
/// constant scenario values) settle and the inventory sees `held_for` actually
/// populated, not merely present-and-null.
const PRESSURE_SETTLE_SAMPLES: u64 = 20;

/// The richest snapshot this test can assemble.
///
/// Starts from [`FakeCollector`] on [`richest_scenario`], then patches in the
/// three things no scenario — and no collector — can produce:
///
/// * **The pressure radar.** No collector derives it; that is
///   [`PressureEngine`]'s job, and the real runtime always runs it over every
///   published snapshot before publishing (`crates/monitrs/src/runtime.rs`).
///   Skipping that step here would leave every `pressure.*` field that carries
///   real substructure silently absent, even though a real export always has it.
/// * **Linux PSI**, via [`fake_psi_snapshot`].
/// * **The CPU time breakdown**, via [`fake_cpu_breakdown`].
///
/// and finally replaces `health` with [`rich_health`]. Multiple samples are taken
/// — not just the two or three `export.rs`'s own tests use — because the pressure
/// engine's hysteresis needs a run of observations before it has an opinion; see
/// [`PRESSURE_SETTLE_SAMPLES`]. Every metric produced here is fresh (`Available`)
/// or one of the always-absent unit variants — see the module doc comment for why
/// [`battery_gone_stale_export_json`] exists alongside this.
fn richest_snapshot() -> SystemSnapshot {
    let mut collector = FakeCollector::new(richest_scenario());
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
    let mut engine = PressureEngine::new(Thresholds::default());

    let mut snapshot = collector.sample(&tick).expect("the fake collector samples");
    snapshot.pressure.psi = MetricState::Available(fake_psi_snapshot());
    snapshot.pressure = engine.observe(&snapshot);

    for sequence in 1..PRESSURE_SETTLE_SAMPLES {
        tick = tick.advance(
            start + Duration::from_secs(sequence),
            SystemTime::UNIX_EPOCH + Duration::from_secs(sequence),
            DueTiers::ALL,
        );
        snapshot = collector.sample(&tick).expect("the fake collector samples");
        snapshot.pressure.psi = MetricState::Available(fake_psi_snapshot());
        snapshot.pressure = engine.observe(&snapshot);
    }

    if let MetricState::Available(usage) = &mut snapshot.cpu.total {
        usage.breakdown = MetricState::Available(fake_cpu_breakdown());
    }
    if let MetricState::Available(cores) = &mut snapshot.cpu.per_core
        && let Some(first_core) = cores.first_mut()
    {
        first_core.breakdown = MetricState::Available(fake_cpu_breakdown());
    }
    snapshot.health = rich_health();

    snapshot
}

/// The same richest snapshot, exported once as-is.
fn richest_export_json(snapshot: &SystemSnapshot) -> String {
    SnapshotExport::new(snapshot, RedactionPolicy::default())
        .to_json()
        .expect("the export serializes")
}

/// A second export of the same snapshot, with the **whole sensor group** carried
/// over from a previous sample instead of freshly measured.
///
/// [`MetricState::into_stale`] (`crates/monitrs-core/src/model/metric.rs:225`) is
/// the collectors' own way of producing this shape — the same call
/// `FakeCollector::age` makes, and the same one Task 3/4's real collectors make
/// for a sensor reading that briefly failed to refresh — so this is not a shape
/// invented for the test.
///
/// Both sensor fields are aged, and *that* is the point rather than a tidying-up.
/// The 1.0.0 review found that this function aged only `sensors.battery`, while
/// `sensors.temperatures` is read on the same cadence and goes stale with it. So at
/// idle the palette's `:export` (`interactive.rs`, serialising
/// `state.live_snapshot()`) emitted `sensors.temperatures.stale.value[].celsius`,
/// which was not one of the paths this inventory recorded, and *omitted*
/// `sensors.temperatures.available[].celsius`, which was. The guard could not see
/// either fact: it aged one field of a group whose two fields always move together.
/// The two are aged together here for the same reason the collectors read them
/// together.
///
/// A snapshot's `sensors.battery` is a single [`MetricState`], not a per-element
/// array like `cpu.per_core`, so it can only be `Available` or `Stale` in any one
/// export, never both at once — there is no equivalent here of leaving one array
/// entry in the other state. Rather than sacrifice `sensors.*.available.*` (which
/// [`richest_snapshot`] already produces, and which a real machine reports on the
/// tick a sensor read actually lands) to gain `sensors.*.stale.*`, this builds a
/// *second* export from a clone with those fields aged, and [`exported_paths`]
/// takes the union of both exports' paths — so neither shape is lost from the
/// inventory for the other's sake.
fn sensors_gone_stale_export_json(snapshot: &SystemSnapshot) -> String {
    let mut snapshot = snapshot.clone();
    let age = Duration::from_secs(45);
    snapshot.sensors.battery = snapshot.sensors.battery.into_stale(age);
    snapshot.sensors.temperatures = snapshot.sensors.temperatures.into_stale(age);
    SnapshotExport::new(&snapshot, RedactionPolicy::default())
        .to_json()
        .expect("the export serializes")
}

/// Inserts every leaf path parsed from `json` into `out`.
fn insert_field_paths(json: &str, out: &mut BTreeSet<String>) {
    let value: Value = serde_json::from_str(json).expect("the export is valid JSON");
    field_paths(&value, "", out);
}

/// Every field path produced by either of this test's two exports.
///
/// The union of the richest all-`Available` export and the sensors-gone-stale
/// export: see the module doc comment and [`sensors_gone_stale_export_json`] for
/// why a single export cannot carry both shapes of the same field.
fn exported_paths() -> BTreeSet<String> {
    let snapshot = richest_snapshot();
    let mut paths = BTreeSet::new();
    insert_field_paths(&richest_export_json(&snapshot), &mut paths);
    insert_field_paths(&sensors_gone_stale_export_json(&snapshot), &mut paths);
    paths
}

#[test]
fn every_promised_field_is_still_exported() {
    let version = SCHEMA_VERSION;
    let recorded = std::fs::read_to_string(inventory_path(version))
        .unwrap_or_else(|error| panic!("{}: {error}", inventory_path(version).display()));
    let recorded: BTreeSet<String> =
        serde_json::from_str(&recorded).expect("the inventory is a JSON array of strings");

    let exported = exported_paths();

    // Printed, not asserted: the promise is about removal, and a consumer reading
    // by name cannot be broken by a field it does not know about. The list is here
    // because it is what a reviewer needs in order to regenerate the inventory
    // deliberately rather than by accident.
    let added: Vec<&String> = exported.difference(&recorded).collect();
    if !added.is_empty() {
        println!("new field paths since the inventory was written: {added:?}");
    }

    let missing: Vec<&String> = recorded.difference(&exported).collect();
    assert!(
        missing.is_empty(),
        "these paths are in docs/schema/v{version}.json but the exporter no longer \
         produces them: {missing:?}\n\
         Either restore the fields, or bump SCHEMA_VERSION and write \
         docs/schema/v{}.json beside the old one — the old file stays, so a \
         consumer can see exactly what changed. Adding a path is not a break; \
         removing or renaming one is.",
        version + 1
    );
}
