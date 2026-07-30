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
/// this proves the factory hands back more than the baseline. Compared against a
/// live [`CommonCollector`] rather than against a hard-coded list of flags, because
/// which capabilities a native layer wins is platform-specific *and*
/// machine-specific — a container without `/proc/pressure` resolves different ones
/// from a desktop kernel, and a list written on a Mac fails on both.
///
/// Both collectors are sampled twice first. Capabilities are not all known at
/// construction: the Linux layer declares most of its during `apply`, which is the
/// honest order — it declares what it *actually* read, not what the platform usually
/// provides (§4). A test that asked before sampling would compare two half-answers.
#[test]
#[cfg_attr(
    not(any(
        all(target_os = "linux", feature = "linux-native"),
        all(target_os = "macos", feature = "macos-native")
    )),
    ignore = "no native layer in this build, so the baseline is the correct answer"
)]
fn the_platform_factory_returns_more_than_the_baseline() {
    use monitrs_collectors::CommonCollector;
    use monitrs_core::model::CapabilityState;

    fn capabilities_after_two_samples(
        collector: &mut impl SnapshotSource,
    ) -> Vec<(&'static str, CapabilityState)> {
        let mut tick = SampleTick::first(std::time::Instant::now(), std::time::SystemTime::now());
        let _ = collector.sample(&tick).expect("a first sample");
        std::thread::sleep(std::time::Duration::from_millis(300));
        tick = tick.advance(
            std::time::Instant::now(),
            std::time::SystemTime::now(),
            DueTiers::ALL,
        );
        let snapshot = collector.sample(&tick).expect("a second sample");
        snapshot.capabilities.entries().to_vec()
    }

    let mut baseline = CommonCollector::new().expect("the baseline must construct");
    let mut platform = platform_collector().expect("the platform collector must construct");
    let bare = capabilities_after_two_samples(&mut baseline);
    let native = capabilities_after_two_samples(&mut platform);

    let mut gained = Vec::new();
    let mut lost = Vec::new();
    for ((name, bare_state), (native_name, native_state)) in bare.iter().zip(native.iter()) {
        assert_eq!(name, native_name, "the entry order must be stable");
        let was = matches!(bare_state, CapabilityState::Available);
        let is = matches!(native_state, CapabilityState::Available);
        if is && !was {
            gained.push(*name);
        }
        if was && !is {
            lost.push((*name, *bare_state, *native_state));
        }
    }

    assert!(
        !gained.is_empty(),
        "the native layer resolves nothing the baseline could not; `platform_collector` \
         is handing back the baseline.\n  baseline: {bare:?}\n  native:   {native:?}"
    );
    // §9.2's enrichment upgrades and does not downgrade. A capability the baseline
    // could report and the native layer cannot is a regression, not a platform fact.
    assert!(
        lost.is_empty(),
        "the native layer took capabilities away from the baseline: {lost:?}"
    );
    println!(
        "native layer resolves {} more capabilities: {gained:?}",
        gained.len()
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
