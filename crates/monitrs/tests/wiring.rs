//! Structural checks on how the binary is wired to its collector.
//!
//! Both bugs pinned here were found by *rendering a frame from the live system*
//! rather than by any unit test, because in both cases every unit was behaving
//! exactly as written: the binary asked for the cross-platform baseline and got
//! it, and the baseline reported what `sysinfo` gave it. What was wrong was the
//! wiring — which no test of a part can see.
//!
//! A source scan is a blunt instrument, and it is used here for one reason: the
//! failure it prevents is silent. Sampling through [`CommonCollector`] compiles,
//! runs, and produces plausible-looking numbers — it just reports a refused read
//! as `0`, understates every capability, and turns the whole native layer of
//! §9.2 into dead code. Nothing short of comparing two collectors' output would
//! notice at runtime, so the check is on the call site instead.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              failure must name the line that broke"
)]

use std::path::{Path, PathBuf};

use monitrs_collectors::{DueTiers, SampleTick, SnapshotSource, platform_collector};

/// The binary's own sources, which are the ones that must not name a collector.
fn binary_sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", directory.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    assert!(
        files.len() > 5,
        "expected the whole binary crate, got {files:?}"
    );
    files
}

#[test]
fn the_binary_reaches_its_collector_only_through_the_platform_factory() {
    // Occurrences inside a comment are allowed: explaining *why* the baseline is
    // not constructed directly is exactly what the call sites should say.
    let forbidden = "CommonCollector::new";
    let mut offenders = Vec::new();
    for path in binary_sources() {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
        for (number, line) in source.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if code.contains(forbidden) {
                offenders.push(format!("{}:{}", path.display(), number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the binary must sample through `platform_collector()`, which adds the \
         native enrichment of §9.2; constructing the baseline directly compiles \
         and runs but reports a refused read as `0`. Found at: {offenders:?}"
    );
}

/// On a platform with a native layer, the factory must actually return it.
///
/// Complements the source scan: the scan proves the binary calls the factory, and
/// this proves the factory hands back more than the baseline. Written against
/// capabilities rather than a type name because capabilities are what the rest of
/// the program reacts to (§4).
#[test]
#[cfg_attr(
    not(any(
        all(target_os = "linux", feature = "linux-native"),
        all(target_os = "macos", feature = "macos-native")
    )),
    ignore = "no native layer in this build, so the baseline is the correct answer"
)]
fn the_platform_factory_returns_more_than_the_baseline() {
    use monitrs_core::model::CapabilityState;

    let collector = platform_collector().expect("the platform collector must construct");
    let capabilities = collector.capabilities();

    // Every native layer resolves at least one thing the baseline cannot. Which
    // ones differ is platform-specific, so the assertion is on the union rather
    // than on a single flag: a build where none of these improved is a build
    // running on the baseline.
    let improved = [
        capabilities.cpu_breakdown,
        capabilities.per_process_open_files,
        capabilities.swap_activity,
        capabilities.linux_psi,
        capabilities.cgroup_limits,
    ]
    .into_iter()
    .filter(|state| matches!(state, CapabilityState::Available))
    .count();

    assert!(
        improved > 0,
        "the native layer resolves none of the capabilities the baseline cannot; \
         `platform_collector` is handing back the baseline. Capabilities: \
         {capabilities:?}"
    );
}

/// A refused per-process read must arrive as `PermissionDenied`, never as a zero.
///
/// §26 states this as a rule and the macOS collector has a unit test for it, but
/// that test constructs the native collector by name — so it kept passing while
/// the shipped binary reported `cpu 0.0%, rss 0` for every process the OS would
/// not talk about. This asserts it of whatever [`platform_collector`] returns.
#[test]
fn the_shipped_collector_never_fabricates_a_zero_for_a_read_it_was_refused() {
    let mut collector = platform_collector().expect("constructs");
    let mut tick = SampleTick::first(std::time::Instant::now(), std::time::SystemTime::now());
    // Two samples: the first cannot compute any rate at all (§8.2), so it would
    // pass this test for the wrong reason.
    let _first = collector.sample(&tick).expect("a first sample");
    std::thread::sleep(std::time::Duration::from_millis(300));
    tick = tick.advance(
        std::time::Instant::now(),
        std::time::SystemTime::now(),
        DueTiers::ALL,
    );
    let snapshot = collector.sample(&tick).expect("a second sample");

    let unreadable: Vec<_> = snapshot
        .processes
        .iter()
        .filter(|process| process.user.fresh().is_none_or(|user| user.uid == 0))
        .collect();
    assert!(
        !unreadable.is_empty(),
        "every OS this runs on has processes an unprivileged user cannot read"
    );

    // A process whose owner could not even be determined, or which belongs to
    // root, and which still claims to use exactly no memory, is reporting a
    // refusal as data. `Available(0)` is a legitimate answer for CPU on an idle
    // process, but not for resident memory of a running one.
    for process in unreadable {
        if let Some(rss) = process.memory.rss_bytes.fresh() {
            assert!(
                *rss > 0,
                "process {} ({}) reports 0 bytes resident as a measured value; a \
                 read the OS refused must be `PermissionDenied` (§26)",
                process.identity.pid,
                process.name
            );
        }
    }
}
